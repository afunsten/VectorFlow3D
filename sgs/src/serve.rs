//! Phase 5.5 — WebSocket transport for the `vf.bridge.v1` observer path
//! (spec §3.5 Transport amendment 2026-07 + §3.6 WebGPU tier).
//!
//! This realizes the Phase 5 [`FakeBridge`](crate::fake_bridge) seams over a
//! **real wire** for the `observer` profile: a browser WebGPU client connects,
//! reconstructs the active set as AABB proxy boxes from diffs, and sends
//! camera/pick/pin back. It is the direct analogue of the scripted `cmd_bridge`
//! demo loop, reshaped from a fixed AOI walk into an **inbound-request-driven**
//! server.
//!
//! Locked transport decisions:
//! - **No tokio.** The server is blocking `tungstenite` over
//!   [`std::net::TcpListener`], **thread-per-connection**, matching the crate's
//!   `ureq` (blocking) ethos. WebRTC/Wilbur stay reserved for the SSR video path.
//! - **One [`Subscription`] per connection.** Each connection owns its own
//!   [`InterestManager`] / [`Rsg`] / [`BridgeServer`] / [`PayloadCache`] /
//!   resolver; the LSG + spatial index are shared read-only and the Twin Overlay
//!   is shared behind a mutex (the only mutation is a pin/unpin write-back).
//!
//! Invariants (spec §3.5 hard rule): the bridge invents no IDs, persists no pins
//! as truth (pins live in the Twin Overlay), and a reconnect resyncs from a
//! fresh snapshot — the server is stateless apart from a per-connection cursor.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tungstenite::{Message, WebSocket};

use crate::bridge::{negotiate, BridgeMsg, BridgeRequest, BridgeServer};
use crate::geomstore::GeomStore;
use crate::hydrate::{PayloadCache, PayloadLoader, StubPayloadLoader, UsdPayloadLoader};
use crate::interest::{
    Interaction, InterestManager, Region, Subscription, SubscriptionId, ViewerProfile,
};
use crate::lsg::Lsg;
use crate::overlay::TwinOverlay;
use crate::resolver::{resolve_active, Resolver, StubResolver, VictoriaMetricsResolver};
use crate::rsg::{Rsg, RsgDiff};
use crate::spatial::SpatialIndex;

/// One subscription per connection (spec §3.2 / the observer camera).
const SUB: SubscriptionId = 1;

/// How to hydrate payloads for a connection. The observer renders AABB proxy
/// boxes (spec §3.6), so hydration is not on the render path — but the RSG still
/// needs a loader; synthetic / NDJSON worlds have no on-disk assets and use the
/// stub, while a real USD root resolves payloads next to the root via the helper.
#[derive(Debug, Clone)]
pub enum PayloadSource {
    Stub,
    Usd { base: PathBuf, python: Option<String> },
}

impl PayloadSource {
    fn loader(&self) -> Box<dyn PayloadLoader> {
        match self {
            PayloadSource::Stub => Box::new(StubPayloadLoader),
            PayloadSource::Usd { base, python } => {
                Box::new(UsdPayloadLoader::new(base.clone(), python.clone()))
            }
        }
    }
}

/// Where telemetry quality (the box tint) comes from. Defaults to an offline
/// stub so the page renders with zero external infra; `Vm` hits a live
/// VictoriaMetrics per AOI update (still the ONLY component that speaks PromQL).
#[derive(Debug, Clone)]
pub enum TelemetryConfig {
    Offline(f64),
    Outage,
    Vm(String),
}

impl TelemetryConfig {
    fn resolver(&self) -> Box<dyn Resolver> {
        match self {
            TelemetryConfig::Offline(v) => Box::new(StubResolver::new(*v)),
            TelemetryConfig::Outage => Box::new(StubResolver::outage()),
            TelemetryConfig::Vm(url) => Box::new(VictoriaMetricsResolver::new(url)),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            TelemetryConfig::Offline(v) => format!("stub (canned {v})"),
            TelemetryConfig::Outage => "stub (simulated outage)".to_string(),
            TelemetryConfig::Vm(url) => format!("VictoriaMetrics {url}"),
        }
    }
}

/// Per-connection knobs (shared, read-only for the lifetime of the server).
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub aoi_radius: f64,
    pub budget: usize,
    pub grace_steps: u64,
    pub telemetry: TelemetryConfig,
    pub payload: PayloadSource,
}

/// State shared across connection threads. The LSG + spatial index are
/// immutable after load (`Arc`); the Twin Overlay is behind a `Mutex` because a
/// pin/unpin write-back is the one mutation any connection performs. The VF
/// geometry store (Phase 5.6) is process-shared behind a `Mutex` so identical
/// payloads tessellate once and a reconnect re-fetches with no redundant work.
pub struct Shared {
    pub lsg: Arc<Lsg>,
    pub index: Arc<SpatialIndex>,
    pub overlay: Arc<Mutex<TwinOverlay>>,
    pub geom_store: Arc<Mutex<GeomStore>>,
    pub config: ServeConfig,
}

/// Wall-clock milliseconds since the epoch (for the resolver TTL cache).
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Bind `addr` and serve WebSocket connections until the process is killed. Each
/// accepted socket gets its own thread and its own subscription.
pub fn run(addr: &str, shared: Shared) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .with_context(|| format!("binding WebSocket listener on {addr}"))?;
    let local = listener.local_addr().ok();
    println!(
        "sgs serve listening on ws://{} — observer WebGPU bridge (vf.bridge.v1)",
        local.map(|a| a.to_string()).unwrap_or_else(|| addr.to_string())
    );
    println!("  telemetry: {}", shared.config.telemetry.describe());
    println!("  entities:  {}", shared.lsg.len());
    run_listener(listener, Arc::new(shared))
}

/// Accept loop over an already-bound listener (thread-per-connection). Split
/// from [`run`] so tests can drive it over an ephemeral loopback port.
pub fn run_listener(listener: TcpListener, shared: Arc<Shared>) -> Result<()> {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            match tungstenite::accept(stream) {
                Ok(mut ws) => {
                    if let Err(e) = serve_connection(&mut ws, &shared) {
                        eprintln!("connection {peer:?} closed: {e:#}");
                    }
                }
                Err(e) => eprintln!("ws handshake failed for {peer:?}: {e}"),
            }
        });
    }
    Ok(())
}

/// Drive one WebSocket connection: negotiate + Hello + snapshot, then map every
/// inbound [`BridgeRequest`] to the matching [`BridgeServer`] method and push
/// the resulting [`BridgeMsg`] batch back down. Generic over the stream so tests
/// can drive it over a loopback socket.
pub fn serve_connection<S: Read + Write>(ws: &mut WebSocket<S>, shared: &Shared) -> Result<()> {
    let lsg = &*shared.lsg;
    let idx = &*shared.index;
    let cfg = &shared.config;

    // ---- 1. Connect handshake (spec §5: negotiate on connect) ----
    let versions = match read_request(ws)? {
        Some(BridgeRequest::Connect { protocol_versions }) => protocol_versions,
        Some(_other) => {
            // Protocol error: the first frame must be Connect.
            anyhow::bail!("first frame was not a Connect handshake");
        }
        None => return Ok(()), // closed before handshake
    };
    let mut server = BridgeServer::new();
    send_batch(ws, &[server.hello(lsg)])?;
    if negotiate(&versions).is_none() {
        // No shared protocol version: Hello already told the client our version;
        // close politely.
        let _ = ws.close(None);
        return Ok(());
    }

    // ---- 2. Per-connection state (one Subscription per connection) ----
    let mut sub = Subscription::spatial(
        SUB,
        Region::Sphere {
            center: [0.0, 0.0, 0.0],
            radius: cfg.aoi_radius,
        },
        cfg.budget,
    );
    // Observer profile: read-only (spec §2.5). A camera AOI still activates.
    sub.viewer_profile = Some(ViewerProfile::Observer);
    sub.interaction = Interaction::ReadOnly;

    let mut im = InterestManager::new();
    im.upsert(sub.clone()).map_err(anyhow::Error::msg)?;
    let mut rsg = Rsg::new(cfg.grace_steps);
    let mut cache = PayloadCache::new(cfg.payload.loader());
    let mut resolver = cfg.telemetry.resolver();
    let mut tick: u64 = 0;

    // ---- 3. Initial snapshot for the (default) AOI ----
    let t = im.evaluate(lsg, idx);
    rsg.apply(&t, lsg, &mut cache, tick)?;
    resolve_active(&mut rsg, lsg, resolver.as_mut(), now_ms());
    // The snapshot supersedes the activation diff; drop the cursor so subsequent
    // diffs are true deltas relative to what the client already has.
    let _ = rsg.take_diff(SUB);
    let snap = {
        let overlay = shared.overlay.lock().unwrap();
        server.snapshot(SUB, lsg, &rsg, &overlay)?
    };
    send_batch(ws, &snap)?;

    // ---- 4. Request loop ----
    while let Some(req) = read_request(ws)? {
        // Geometry fetch (Phase 5.6): answer with the GLB bytes as a single
        // binary frame — an empty frame when the mesh is absent/unknown so the
        // client's per-connection FIFO stays aligned and it keeps the proxy box.
        if let BridgeRequest::FetchGeom { content_hash } = &req {
            let bytes = {
                let mut store = shared.geom_store.lock().unwrap();
                store.fetch(content_hash)?
            };
            let payload = bytes.map(|a| a.as_ref().clone()).unwrap_or_default();
            send_binary(ws, payload)?;
            continue;
        }
        let out = handle_request(
            req,
            &mut sub,
            &mut im,
            &mut rsg,
            &mut cache,
            resolver.as_mut(),
            &mut server,
            lsg,
            idx,
            shared,
            &mut tick,
        )?;
        send_batch(ws, &out)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    req: BridgeRequest,
    sub: &mut Subscription,
    im: &mut InterestManager,
    rsg: &mut Rsg,
    cache: &mut PayloadCache,
    resolver: &mut dyn Resolver,
    server: &mut BridgeServer,
    lsg: &Lsg,
    idx: &SpatialIndex,
    shared: &Shared,
    tick: &mut u64,
) -> Result<Vec<BridgeMsg>> {
    match req {
        BridgeRequest::Connect { .. } => Ok(vec![server.hello(lsg)]),

        BridgeRequest::UpdateAoi { region, .. } => {
            sub.region = Some(region.to_region());
            im.upsert(sub.clone()).map_err(anyhow::Error::msg)?;
            Ok(reevaluate(sub, im, rsg, cache, resolver, server, lsg, idx, shared, tick)?)
        }

        BridgeRequest::SubscribeExtras { entity_ids, .. } => {
            for id in entity_ids {
                if !sub.entity_ids.contains(&id) {
                    sub.entity_ids.push(id);
                }
            }
            im.upsert(sub.clone()).map_err(anyhow::Error::msg)?;
            Ok(reevaluate(sub, im, rsg, cache, resolver, server, lsg, idx, shared, tick)?)
        }

        BridgeRequest::PickRequest {
            request_id,
            origin,
            dir,
        } => Ok(vec![server.coarse_pick(SUB, request_id, origin, dir, lsg, rsg)]),

        BridgeRequest::PinPart { id, transform } => {
            let Some(prim) = lsg.get(id).map(|e| e.prim_path.clone()) else {
                return Ok(vec![]);
            };
            let mut out = Vec::new();
            {
                let mut overlay = shared.overlay.lock().unwrap();
                let confirm = server.handle_pin(id, &prim, transform, Some("observer"), &mut overlay)?;
                out.push(confirm);
            }
            // Re-upsert the pinned entity so the live box snaps to the pin.
            let overlay = shared.overlay.lock().unwrap();
            let redraw = RsgDiff {
                upserts: vec![id],
                removes: vec![],
            };
            out.extend(server.encode_diff(SUB, &redraw, lsg, rsg, &overlay)?);
            Ok(out)
        }

        BridgeRequest::UnpinPart { id } => {
            let Some(entity) = lsg.get(id) else {
                return Ok(vec![]);
            };
            let authored = entity.transform_default;
            let mut out = Vec::new();
            {
                let mut overlay = shared.overlay.lock().unwrap();
                out.push(server.handle_unpin(id, authored, &mut overlay)?);
            }
            let overlay = shared.overlay.lock().unwrap();
            let redraw = RsgDiff {
                upserts: vec![id],
                removes: vec![],
            };
            out.extend(server.encode_diff(SUB, &redraw, lsg, rsg, &overlay)?);
            Ok(out)
        }

        BridgeRequest::Heartbeat { budget, .. } => {
            if budget > 0 {
                sub.budget = budget;
                im.upsert(sub.clone()).map_err(anyhow::Error::msg)?;
            }
            Ok(vec![])
        }

        // Handled in the request loop with a binary frame (see serve_connection).
        BridgeRequest::FetchGeom { .. } => Ok(vec![]),
    }
}

/// Re-evaluate interest after the subscription changed, refresh telemetry, and
/// encode this subscriber's diff (spec §3.3 per-subscriber cursor).
#[allow(clippy::too_many_arguments)]
fn reevaluate(
    _sub: &Subscription,
    im: &mut InterestManager,
    rsg: &mut Rsg,
    cache: &mut PayloadCache,
    resolver: &mut dyn Resolver,
    server: &mut BridgeServer,
    lsg: &Lsg,
    idx: &SpatialIndex,
    shared: &Shared,
    tick: &mut u64,
) -> Result<Vec<BridgeMsg>> {
    *tick += 1;
    let now = *tick;
    let t = im.evaluate(lsg, idx);
    rsg.apply(&t, lsg, cache, now)?;
    rsg.evict_expired(now, cache);
    resolve_active(rsg, lsg, resolver, now_ms());
    let diff = rsg.take_diff(SUB);
    let overlay = shared.overlay.lock().unwrap();
    server.encode_diff(SUB, &diff, lsg, rsg, &overlay)
}

/// Read one inbound [`BridgeRequest`], transparently skipping WebSocket control
/// frames. Returns `Ok(None)` when the peer closes.
fn read_request<S: Read + Write>(ws: &mut WebSocket<S>) -> Result<Option<BridgeRequest>> {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let req: BridgeRequest = serde_json::from_str(t.as_str())
                    .with_context(|| format!("parsing inbound BridgeRequest: {t}"))?;
                return Ok(Some(req));
            }
            Ok(Message::Binary(b)) => {
                let req: BridgeRequest = serde_json::from_slice(&b)
                    .context("parsing inbound binary BridgeRequest")?;
                return Ok(Some(req));
            }
            Ok(Message::Close(_)) => return Ok(None),
            // Ping/Pong/Frame are handled by tungstenite internally; keep reading.
            Ok(_) => continue,
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                return Ok(None)
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Send a batch of [`BridgeMsg`] as a single JSON-array text frame (the client
/// and the Rust `FakeBridge` both apply `Vec<BridgeMsg>`). Empty batches are
/// skipped so a no-op request produces no frame.
fn send_batch<S: Read + Write>(ws: &mut WebSocket<S>, msgs: &[BridgeMsg]) -> Result<()> {
    if msgs.is_empty() {
        return Ok(());
    }
    let json = serde_json::to_string(msgs).context("serializing BridgeMsg batch")?;
    ws.send(Message::text(json)).context("sending BridgeMsg batch")?;
    Ok(())
}

/// Send raw bytes as a single binary WebSocket frame (Phase 5.6 GLB mesh
/// delivery). An empty payload is a valid "mesh absent" reply.
fn send_binary<S: Read + Write>(ws: &mut WebSocket<S>, bytes: Vec<u8>) -> Result<()> {
    ws.send(Message::binary(bytes)).context("sending GLB binary frame")?;
    Ok(())
}

/// Convenience for `TcpStream`-backed connections (the production path).
pub fn serve_tcp(ws: &mut WebSocket<TcpStream>, shared: &Shared) -> Result<()> {
    serve_connection(ws, shared)
}

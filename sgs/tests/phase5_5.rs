//! Phase 5.5 integration tests: the `vf.bridge.v1` stream over a real
//! WebSocket (spec §3.5 Transport amendment 2026-07 / §3.6 WebGPU tier).
//!
//! A blocking `sgs serve` listener runs on an ephemeral loopback port; a
//! `tungstenite` client connects, negotiates, and drives the same
//! Connect/UpdateAoi/PinPart requests the browser observer sends. Frames are fed
//! into the Rust [`FakeBridge`] (the same cache the TypeScript client ports) to
//! prove reconstruction over the wire matches the in-process snapshot and that
//! the Phase 5 invariants still hold: reconnect resyncs identically, a pin
//! survives reconnect via the Twin Overlay, and the LSG is never mutated (zero
//! USD writes).

use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use tungstenite::{Message, WebSocket};

use vectorflow_sgs::bridge::{BridgeMsg, BridgeRequest, BridgeServer, PROTOCOL_VERSION};
use vectorflow_sgs::fake_bridge::FakeBridge;
use vectorflow_sgs::import::build_from_ndjson;
use vectorflow_sgs::interest::{InterestManager, Region, Subscription, SubscriptionId};
use vectorflow_sgs::lsg::{EntityId, Lsg, Transform};
use vectorflow_sgs::overlay::TwinOverlay;
use vectorflow_sgs::rsg::Rsg;
use vectorflow_sgs::serve::{PayloadSource, ServeConfig, Shared, TelemetryConfig};
use vectorflow_sgs::spatial::SpatialIndex;
use vectorflow_sgs::{hydrate::PayloadCache, hydrate::StubPayloadLoader};

const SUB: SubscriptionId = 1;
const AOI_RADIUS: f64 = 8.0;
const BUDGET: usize = 1000;

/// A line of `n` payload-backed components along +x near the origin.
fn line_world_ndjson(n: usize, spacing: f64) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for i in 0..n {
        let x = i as f64 * spacing;
        writeln!(
            s,
            r#"{{"primPath":"/W/e{i}","kind":"component","transform":{{"translate":[{x},0,0]}},"extentsHint":[[{lo},-0.5,-0.5],[{hi},0.5,0.5]],"vf":{{"assetTag":"E{i}"}},"tags":["pump"],"geomRef":{{"payloadUri":"./components/pump.usda","primPath":"/C","contentHash":"h1"}},"bindings":[{{"attribute":"flow","sourceId":"victoriametrics","query":"pump_flow_gpm{{asset=\"E{i}\"}}","unit":"gpm","ttlMs":5000,"priority":"background","qualityPolicy":"stale_ok"}}]}}"#,
            lo = x - 0.5,
            hi = x + 0.5,
        )
        .unwrap();
    }
    s
}

fn shared_world() -> (Arc<Lsg>, Arc<Shared>) {
    let lsg = Arc::new(build_from_ndjson(line_world_ndjson(8, 2.0).as_bytes()).unwrap());
    let index = Arc::new(SpatialIndex::build(&lsg));
    let overlay = Arc::new(Mutex::new(TwinOverlay::open_in_memory().unwrap()));
    let shared = Arc::new(Shared {
        lsg: Arc::clone(&lsg),
        index,
        overlay,
        geom_store: Arc::new(Mutex::new(
            vectorflow_sgs::geomstore::GeomStore::from_lsg(
                Box::new(vectorflow_sgs::geomstore::StubMeshLoader),
                &lsg,
            ),
        )),
        config: ServeConfig {
            aoi_radius: AOI_RADIUS,
            budget: BUDGET,
            grace_steps: 2,
            telemetry: TelemetryConfig::Offline(42.0),
            payload: PayloadSource::Stub,
        },
    });
    (lsg, shared)
}

/// Spawn the blocking accept loop on an ephemeral port; returns the address.
fn spawn_server(shared: Arc<Shared>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        let _ = vectorflow_sgs::serve::run_listener(listener, shared);
    });
    addr
}

fn connect(addr: &str) -> WebSocket<TcpStream> {
    // Retry briefly in case the accept loop hasn't started listening yet.
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(addr) {
            if let Ok((ws, _resp)) = tungstenite::client(format!("ws://{addr}/"), stream) {
                return ws;
            }
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("could not connect to {addr}");
}

fn send(ws: &mut WebSocket<TcpStream>, req: &BridgeRequest) {
    ws.send(Message::text(serde_json::to_string(req).unwrap()))
        .unwrap();
}

/// Read the next non-control frame as a decoded batch of `BridgeMsg`.
fn read_batch(ws: &mut WebSocket<TcpStream>) -> Vec<BridgeMsg> {
    loop {
        match ws.read().unwrap() {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Binary(b) => return serde_json::from_slice(&b).unwrap(),
            _ => continue,
        }
    }
}

/// What the server should expose for the default AOI (center origin), computed
/// in-process the same way `serve_connection` does, so the wire reconstruction
/// can be compared to it.
fn expected_snapshot(lsg: &Lsg, overlay: &TwinOverlay) -> Vec<BridgeMsg> {
    let idx = SpatialIndex::build(lsg);
    let mut sub = Subscription::spatial(
        SUB,
        Region::Sphere {
            center: [0.0, 0.0, 0.0],
            radius: AOI_RADIUS,
        },
        BUDGET,
    );
    sub.entity_ids = vec![];
    let mut im = InterestManager::new();
    im.upsert(sub).unwrap();
    let mut rsg = Rsg::new(2);
    let mut cache = PayloadCache::new(Box::new(StubPayloadLoader));
    let t = im.evaluate(lsg, &idx);
    rsg.apply(&t, lsg, &mut cache, 0).unwrap();
    let mut server = BridgeServer::new();
    server.snapshot(SUB, lsg, &rsg, overlay).unwrap()
}

fn reconstruct(batches: &[Vec<BridgeMsg>]) -> FakeBridge {
    let mut fb = FakeBridge::new();
    for b in batches {
        fb.apply(b);
    }
    fb
}

#[test]
fn connect_hello_snapshot_over_the_wire() {
    let (lsg, shared) = shared_world();
    let addr = spawn_server(Arc::clone(&shared));
    let mut ws = connect(&addr);

    send(&mut ws, &BridgeRequest::Connect {
        protocol_versions: vec![PROTOCOL_VERSION.to_string()],
    });

    // Frame 1: [Hello]. Frame 2: the snapshot batch.
    let hello = read_batch(&mut ws);
    assert!(matches!(hello.as_slice(), [BridgeMsg::Hello { protocol, .. }] if protocol == PROTOCOL_VERSION));
    let snap = read_batch(&mut ws);
    assert!(matches!(snap.first(), Some(BridgeMsg::SnapshotBegin { .. })));
    assert!(matches!(snap.last(), Some(BridgeMsg::SnapshotMarker { .. })));

    // Reconstruct over the wire and compare to the in-process snapshot set.
    let fb = reconstruct(&[hello, snap]);
    let expected = {
        let ov = shared.overlay.lock().unwrap();
        expected_snapshot(&lsg, &ov)
    };
    let mut expected_ids: Vec<EntityId> = expected
        .iter()
        .filter_map(|m| match m {
            BridgeMsg::UpsertEntity { id, extents, .. } => {
                assert!(extents.is_some(), "proxy box extents must ride the wire");
                Some(*id)
            }
            _ => None,
        })
        .collect();
    expected_ids.sort();
    assert!(!expected_ids.is_empty(), "default AOI should activate some boxes");
    assert_eq!(fb.entity_ids(), expected_ids, "wire reconstruction == snapshot");
    // Every reconstructed box carries origin-relative extents + telemetry tint.
    for id in fb.entity_ids() {
        let e = fb.get(id).unwrap();
        assert!(e.extents.is_some(), "reconstructed box needs extents");
        assert!(!e.visual.is_empty(), "offline stub tint should attach ok telemetry");
    }

    let _ = ws.close(None);
}

#[test]
fn update_aoi_streams_a_diff() {
    let (_lsg, shared) = shared_world();
    let addr = spawn_server(Arc::clone(&shared));
    let mut ws = connect(&addr);

    send(&mut ws, &BridgeRequest::Connect {
        protocol_versions: vec![PROTOCOL_VERSION.to_string()],
    });
    let hello = read_batch(&mut ws);
    let snap = read_batch(&mut ws);
    let mut fb = reconstruct(&[hello, snap]);
    let before = fb.entity_ids();

    // Move the AOI far down +x so a different active set is streamed as a diff.
    send(&mut ws, &BridgeRequest::UpdateAoi {
        subscription: SUB,
        region: vectorflow_sgs::bridge::RegionWire::Sphere {
            center: [12.0, 0.0, 0.0],
            radius: AOI_RADIUS,
        },
    });
    let diff = read_batch(&mut ws);
    assert!(
        diff.iter().any(|m| matches!(m, BridgeMsg::UpsertEntity { .. } | BridgeMsg::RemoveEntity { .. })),
        "moving the AOI must stream upserts/removes"
    );
    fb.apply(&diff);
    let after = fb.entity_ids();
    assert_ne!(before, after, "the active set changes when the camera AOI moves");

    let _ = ws.close(None);
}

#[test]
fn pin_over_the_wire_survives_reconnect_zero_lsg_mutation() {
    let (lsg, shared) = shared_world();
    let start_rev = lsg.revision();
    let addr = spawn_server(Arc::clone(&shared));

    // ---- Connection A: connect, pin an active entity, capture the scene. ----
    let mut a = connect(&addr);
    send(&mut a, &BridgeRequest::Connect {
        protocol_versions: vec![PROTOCOL_VERSION.to_string()],
    });
    let hello_a = read_batch(&mut a);
    let snap_a = read_batch(&mut a);
    let mut fb_a = reconstruct(&[hello_a, snap_a]);

    // Pick a box in the active set and pin it (raise it in z).
    let target = *fb_a.entity_ids().first().expect("active set non-empty");
    let cur = fb_a.get(target).unwrap().transform.translation();
    let pinned = Transform::from_translation([cur[0], cur[1], cur[2] + 8.0]);
    send(&mut a, &BridgeRequest::PinPart { id: target, transform: pinned });

    // The server replies with a PinConfirm + a re-upsert reflecting the pin.
    let pin_batch = read_batch(&mut a);
    assert!(
        pin_batch.iter().any(|m| matches!(m, BridgeMsg::PinConfirm { id, .. } if *id == target)),
        "pin must be confirmed"
    );
    fb_a.apply(&pin_batch);
    assert_eq!(
        fb_a.get(target).unwrap().transform.translation(),
        [cur[0], cur[1], cur[2] + 8.0],
        "the pinned box snaps to the confirmed transform"
    );
    let after_pin_a: Vec<(EntityId, Option<_>)> = fb_a
        .entity_ids()
        .into_iter()
        .map(|id| (id, fb_a.get(id).map(|e| e.transform.translation())))
        .collect();
    let _ = a.close(None);

    // ---- Connection B: reconnect fresh; snapshot must rebuild identically. ----
    let mut b = connect(&addr);
    send(&mut b, &BridgeRequest::Connect {
        protocol_versions: vec![PROTOCOL_VERSION.to_string()],
    });
    let hello_b = read_batch(&mut b);
    let snap_b = read_batch(&mut b);
    let fb_b = reconstruct(&[hello_b, snap_b]);
    let b_state: Vec<(EntityId, Option<_>)> = fb_b
        .entity_ids()
        .into_iter()
        .map(|id| (id, fb_b.get(id).map(|e| e.transform.translation())))
        .collect();

    assert_eq!(after_pin_a, b_state, "reconnect reconstructs the scene identically");
    // The pin survived because it lives in the Twin Overlay, not the session.
    assert_eq!(
        fb_b.get(target).unwrap().transform.translation(),
        [cur[0], cur[1], cur[2] + 8.0]
    );
    // The bridge never mutates the LSG index (proxy for zero USD writes).
    assert_eq!(lsg.revision(), start_rev, "serving never mutates the LSG");

    let _ = b.close(None);
}

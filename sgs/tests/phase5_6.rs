//! Phase 5.6 integration tests: the VF geometry store + GLB mesh delivery over
//! the `vf.bridge.v1` WebSocket (spec §3.1 / §4.2 / §3.6 / Phase 5.6).
//!
//! Two paths are covered:
//! - A **Python-free loopback WS** test: a canned mesh loader seeds the store for
//!   a known hash, and `FetchGeom` returns a valid GLB binary frame for it while
//!   an unknown hash yields an empty frame (the observer keeps its proxy box).
//! - A **USD-gated** store test: the real `pump-station-01` fixture tessellates
//!   its three component payloads into three deduplicated, non-empty store
//!   entries (tessellate once), with `USD writes: 0` (the LSG revision is
//!   unchanged). It is skipped when the OpenUSD toolchain (`tools/.venv`) is
//!   absent, mirroring how the import path shells out to the helper.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use tungstenite::{Message, WebSocket};

use vectorflow_sgs::bridge::{BridgeMsg, BridgeRequest, PROTOCOL_VERSION};
use vectorflow_sgs::geomstore::{decode_glb, GeomStore, Mesh, MeshLoader, UsdMeshLoader};
use vectorflow_sgs::import::{build_from_ndjson, import_from_usd};
use vectorflow_sgs::lsg::{GeomRef, Lsg};
use vectorflow_sgs::overlay::TwinOverlay;
use vectorflow_sgs::serve::{PayloadSource, ServeConfig, Shared, TelemetryConfig};
use vectorflow_sgs::spatial::SpatialIndex;

const AOI_RADIUS: f64 = 8.0;
const BUDGET: usize = 1000;

/// A canned loader that returns a fixed triangle mesh for known content hashes.
/// Keeps the loopback test Python-free while exercising the real store + wire.
struct CannedMeshLoader {
    known: Vec<String>,
}

impl MeshLoader for CannedMeshLoader {
    fn load(&self, geom: &GeomRef) -> Result<Option<Mesh>> {
        if self.known.contains(&geom.content_hash) {
            Ok(Some(Mesh {
                positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
                indices: vec![0, 1, 2],
            }))
        } else {
            Ok(None)
        }
    }
}

/// A small world of payload-backed components near the origin, all sharing the
/// content hash `h1` (so the default AOI activates them).
fn line_world_ndjson(n: usize, spacing: f64) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for i in 0..n {
        let x = i as f64 * spacing;
        writeln!(
            s,
            r#"{{"primPath":"/W/e{i}","kind":"component","transform":{{"translate":[{x},0,0]}},"extentsHint":[[{lo},-0.5,-0.5],[{hi},0.5,0.5]],"vf":{{"assetTag":"E{i}"}},"tags":["pump"],"geomRef":{{"payloadUri":"./components/pump.usda","primPath":"/Pump","contentHash":"h1"}}}}"#,
            lo = x - 0.5,
            hi = x + 0.5,
        )
        .unwrap();
    }
    s
}

fn shared_with_canned_store() -> Arc<Shared> {
    let lsg = Arc::new(build_from_ndjson(line_world_ndjson(6, 2.0).as_bytes()).unwrap());
    let index = Arc::new(SpatialIndex::build(&lsg));
    let overlay = Arc::new(Mutex::new(TwinOverlay::open_in_memory().unwrap()));
    let store = GeomStore::from_lsg(
        Box::new(CannedMeshLoader {
            known: vec!["h1".to_string()],
        }),
        &lsg,
    );
    Arc::new(Shared {
        lsg,
        index,
        overlay,
        geom_store: Arc::new(Mutex::new(store)),
        config: ServeConfig {
            aoi_radius: AOI_RADIUS,
            budget: BUDGET,
            grace_steps: 2,
            telemetry: TelemetryConfig::Offline(42.0),
            payload: PayloadSource::Stub,
        },
    })
}

fn spawn_server(shared: Arc<Shared>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        let _ = vectorflow_sgs::serve::run_listener(listener, shared);
    });
    addr
}

fn connect(addr: &str) -> WebSocket<TcpStream> {
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

/// Next text (JSON batch) frame, skipping control/binary frames.
fn read_batch(ws: &mut WebSocket<TcpStream>) -> Vec<BridgeMsg> {
    loop {
        match ws.read().unwrap() {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Binary(b) => return serde_json::from_slice(&b).unwrap(),
            _ => continue,
        }
    }
}

/// Next binary frame's bytes, skipping control frames.
fn read_binary(ws: &mut WebSocket<TcpStream>) -> Vec<u8> {
    loop {
        match ws.read().unwrap() {
            Message::Binary(b) => return b.to_vec(),
            Message::Text(_) => continue,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(_) => return Vec::new(),
        }
    }
}

#[test]
fn fetch_geom_returns_glb_for_known_hash_and_empty_for_unknown() {
    let shared = shared_with_canned_store();
    let addr = spawn_server(Arc::clone(&shared));
    let mut ws = connect(&addr);

    send(
        &mut ws,
        &BridgeRequest::Connect {
            protocol_versions: vec![PROTOCOL_VERSION.to_string()],
        },
    );
    let hello = read_batch(&mut ws);
    assert!(matches!(hello.as_slice(), [BridgeMsg::Hello { .. }]));
    let snap = read_batch(&mut ws);

    // A snapshot upsert must carry the geom ref whose hash we can fetch.
    let hash = snap
        .iter()
        .find_map(|m| match m {
            BridgeMsg::UpsertEntity {
                geom_ref: Some(g), ..
            } if !g.content_hash.is_empty() => Some(g.content_hash.clone()),
            _ => None,
        })
        .expect("an active entity should carry a GeomRef content hash");
    assert_eq!(hash, "h1");

    // Known hash: a valid GLB binary frame that decodes to a positive vertex count.
    send(&mut ws, &BridgeRequest::FetchGeom { content_hash: hash });
    let glb = read_binary(&mut ws);
    assert!(!glb.is_empty(), "known hash must return GLB bytes");
    assert_eq!(&glb[0..4], b"glTF", "binary frame is a GLB");
    let mesh = decode_glb(&glb).expect("valid GLB");
    assert!(mesh.vertex_count() > 0, "mesh has vertices");

    // Unknown hash: an empty frame (client keeps its proxy box).
    send(
        &mut ws,
        &BridgeRequest::FetchGeom {
            content_hash: "does-not-exist".to_string(),
        },
    );
    let empty = read_binary(&mut ws);
    assert!(empty.is_empty(), "unknown hash returns an empty frame");

    let _ = ws.close(None);
}

/// The one-time OpenUSD venv the import path shells out to. When it is absent we
/// skip the tessellation test (there is no other USD-gated test yet to mirror).
fn usd_toolchain_available() -> bool {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tools/.venv/bin/python3")
        .exists()
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../assets/usd/pump-station-01/pump_station.usda")
}

#[test]
fn pump_station_tessellates_to_three_deduplicated_meshes_zero_usd_writes() {
    if !usd_toolchain_available() {
        eprintln!("skipping: OpenUSD toolchain (sgs/tools/.venv) not installed");
        return;
    }
    let root = fixture_root();
    let lsg: Lsg = import_from_usd(&root, None).expect("import pump-station via helper");
    let start_rev = lsg.revision();

    let base = root.parent().unwrap().to_path_buf();
    let mut store = GeomStore::from_lsg(Box::new(UsdMeshLoader::new(base, None)), &lsg);

    // Three distinct component files (pump / tank / distribution_switch) => three
    // content hashes, even though seven instances reference them.
    assert_eq!(
        store.known_hashes(),
        3,
        "pump/tank/switch => three deduplicated store entries"
    );

    // Fetch every known hash: each tessellates to a non-empty mesh, once.
    let hashes: Vec<String> = lsg
        .entities()
        .filter_map(|e| e.geom_ref.as_ref())
        .filter(|g| !g.content_hash.is_empty())
        .map(|g| g.content_hash.clone())
        .collect();
    let mut distinct: Vec<String> = hashes.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 3);

    for h in &distinct {
        let glb = store.fetch(h).unwrap().expect("real component tessellates");
        let mesh = decode_glb(&glb).expect("valid GLB");
        assert!(mesh.vertex_count() > 0, "tessellated mesh is non-empty");
        assert!(!mesh.indices.is_empty());
    }
    // Fetching all instances' hashes again is served from cache (tessellate once).
    for h in &hashes {
        let _ = store.fetch(h).unwrap();
    }
    assert_eq!(store.cached_count(), 3);
    assert_eq!(store.load_calls(), 3, "each unique payload tessellates once");

    // The store is derived + read-only: it never mutates the LSG (USD writes: 0).
    assert_eq!(lsg.revision(), start_rev, "geometry store never mutates the LSG");
}

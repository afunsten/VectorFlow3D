//! Phase 5 integration tests: Renderer Bridge API (`vf.bridge.v1`) + fake bridge.
//!
//! End-to-end pipeline (LSG -> InterestManager -> Rsg -> BridgeServer ->
//! FakeBridge), all Python-free via the stub payload loader and NDJSON worlds.
//! Proves the spec §7 Phase 5 gates: a fake bridge reconstructs a scene from the
//! diff stream, a reconnect resyncs identically from a snapshot, pin/unpin
//! write-back flows through the Twin Overlay with zero USD/LSG mutation, coarse
//! ray-AABB pick returns the nearest hit, `GeomRef`s carry USD URIs that point
//! at the fixture, and the protocol is negotiated on connect.

use std::fmt::Write as _;
use std::path::PathBuf;

use vectorflow_sgs::bridge::{
    negotiate, ray_aabb, BridgeMsg, BridgeServer, RegionWire, PROTOCOL_VERSION,
};
use vectorflow_sgs::fake_bridge::FakeBridge;
use vectorflow_sgs::hydrate::{PayloadCache, StubPayloadLoader};
use vectorflow_sgs::import::build_from_ndjson;
use vectorflow_sgs::interest::{InterestManager, Region, Subscription, SubscriptionId};
use vectorflow_sgs::lsg::{Aabb, EntityId, Lsg, Transform};
use vectorflow_sgs::overlay::TwinOverlay;
use vectorflow_sgs::rsg::Rsg;
use vectorflow_sgs::spatial::SpatialIndex;

const SUB: SubscriptionId = 1;

/// A line of `n` payload-backed components spaced `spacing` apart along +x, all
/// referencing the same fixture-relative payload URI so the cache can dedupe.
fn line_world_ndjson(n: usize, spacing: f64, payload_uri: &str) -> String {
    let mut s = String::new();
    for i in 0..n {
        let x = i as f64 * spacing;
        writeln!(
            s,
            r#"{{"primPath":"/W/e{i}","kind":"component","transform":{{"translate":[{x},0,0]}},"extentsHint":[[{lo},-0.5,-0.5],[{hi},0.5,0.5]],"vf":{{"assetTag":"E{i}"}},"tags":["pump"],"geomRef":{{"payloadUri":"{payload_uri}","primPath":"/C","contentHash":"h1"}},"bindings":[]}}"#,
            lo = x - 0.5,
            hi = x + 0.5,
        )
        .unwrap();
    }
    s
}

fn line_world(n: usize, spacing: f64, payload_uri: &str) -> Lsg {
    build_from_ndjson(line_world_ndjson(n, spacing, payload_uri).as_bytes()).unwrap()
}

fn cache() -> PayloadCache {
    PayloadCache::new(Box::new(StubPayloadLoader))
}

/// Build the interest pipeline for a static sphere AOI and drain one step,
/// returning everything the tests need.
fn pipeline(
    lsg: &Lsg,
    center: [f64; 3],
    radius: f64,
    selection: &[EntityId],
) -> (InterestManager, Rsg, PayloadCache) {
    let idx = SpatialIndex::build(lsg);
    let mut sub = Subscription::spatial(SUB, Region::Sphere { center, radius }, 1000);
    sub.entity_ids = selection.to_vec();
    let mut im = InterestManager::new();
    im.upsert(sub).unwrap();
    let mut rsg = Rsg::new(2);
    let mut c = cache();
    let t = im.evaluate(lsg, &idx);
    rsg.apply(&t, lsg, &mut c, 0).unwrap();
    (im, rsg, c)
}

fn active_ids(rsg: &Rsg) -> Vec<EntityId> {
    let mut ids: Vec<EntityId> = rsg
        .entities()
        .filter(|re| re.subscribers.contains(&SUB))
        .map(|re| re.id)
        .collect();
    ids.sort();
    ids
}

#[test]
fn fake_bridge_reconstructs_scene_from_diff_stream() {
    // 6 components in a tight AOI; the bridge should mirror the active set.
    let lsg = line_world(6, 2.0, "./components/pump.usda");
    let (im, rsg, mut c) = pipeline(&lsg, [4.0, 0.0, 0.0], 5.0, &[]);
    let overlay = TwinOverlay::open_in_memory().unwrap();

    let mut server = BridgeServer::new();
    let mut bridge = FakeBridge::new();
    bridge.apply(&[server.hello(&lsg)]);
    assert_eq!(bridge.protocol.as_deref(), Some(PROTOCOL_VERSION));

    // Drain sub 1's diff (initial activation) and reconstruct from it.
    let mut rsg_mut = rsg;
    let diff = rsg_mut.take_diff(SUB);
    let msgs = server
        .encode_diff(SUB, &diff, &lsg, &rsg_mut, &overlay)
        .unwrap();

    // Golden shape: every message is an UpsertEntity (no removes on activation),
    // one per active entity, each carrying a geom ref + tags + resolved xform.
    let expected = active_ids(&rsg_mut);
    assert_eq!(msgs.len(), expected.len());
    let mut seen = Vec::new();
    for m in &msgs {
        match m {
            BridgeMsg::UpsertEntity {
                id,
                geom_ref,
                tags,
                transform,
                ..
            } => {
                assert!(geom_ref.is_some(), "geom ref must carry the USD payload URI");
                assert_eq!(tags, &vec!["pump".to_string()]);
                // authored transform (no pin) resolves through.
                let authored = lsg.get(*id).unwrap().transform_default;
                assert_eq!(transform.translation(), authored.translation());
                seen.push(*id);
            }
            other => panic!("unexpected message on activation: {other:?}"),
        }
    }
    seen.sort();
    assert_eq!(seen, expected);

    bridge.apply(&msgs);
    let _ = bridge.hydrate(&mut c).unwrap();

    // The reconstructed Render Scene equals the subscription's active set.
    assert_eq!(bridge.entity_ids(), expected);
    assert_eq!(bridge.len(), im.active_count(SUB));
    assert_eq!(bridge.hydrated_count(), bridge.len());
    // One distinct payload, hydrated once despite many instances.
    assert_eq!(c.load_calls(), 1);
}

#[test]
fn extents_ride_the_wire_origin_relative() {
    // Each component's authored extentsHint is a world AABB centered on its
    // translate; the emitted UpsertEntity carries that box re-expressed relative
    // to the authored origin (spec §3.6 proxy box), and it survives a JSON
    // round-trip (additive vf.bridge.v1 field).
    let lsg = line_world(4, 2.0, "./components/pump.usda");
    let (_im, mut rsg, _c) = pipeline(&lsg, [4.0, 0.0, 0.0], 100.0, &[]);
    let overlay = TwinOverlay::open_in_memory().unwrap();
    let mut server = BridgeServer::new();
    let diff = rsg.take_diff(SUB);
    let msgs = server.encode_diff(SUB, &diff, &lsg, &rsg, &overlay).unwrap();

    let mut checked = 0;
    for m in &msgs {
        if let BridgeMsg::UpsertEntity { extents, .. } = m {
            let e = extents.expect("proxy box extents must ride the wire");
            // line_world authors [x-0.5,..]..[x+0.5,..]; origin-relative => ±0.5.
            assert!((e.min[0] - -0.5).abs() < 1e-9, "min.x={:?}", e.min);
            assert!((e.max[0] - 0.5).abs() < 1e-9, "max.x={:?}", e.max);
            checked += 1;
        }
    }
    assert!(checked > 0, "expected extents-bearing upserts");

    // Additive field round-trips through JSON.
    let json = serde_json::to_string(&msgs).unwrap();
    let back: Vec<BridgeMsg> = serde_json::from_str(&json).unwrap();
    assert_eq!(msgs, back);
}

#[test]
fn snapshot_equals_snapshot_plus_diffs() {
    let lsg = line_world(5, 2.0, "./components/pump.usda");
    let (_im, mut rsg, _c) = pipeline(&lsg, [4.0, 0.0, 0.0], 5.0, &[]);
    let overlay = TwinOverlay::open_in_memory().unwrap();

    // Bridge A: reconstruct from the live diff stream.
    let mut sa = BridgeServer::new();
    let mut a = FakeBridge::new();
    let diff = rsg.take_diff(SUB);
    a.apply(&sa.encode_diff(SUB, &diff, &lsg, &rsg, &overlay).unwrap());

    // Bridge B: reconstruct from a full snapshot.
    let mut sb = BridgeServer::new();
    let mut b = FakeBridge::new();
    b.apply(&sb.snapshot(SUB, &lsg, &rsg, &overlay).unwrap());

    // Same entities, same resolved state.
    assert_eq!(a.entity_ids(), b.entity_ids());
    for id in a.entity_ids() {
        assert_eq!(a.get(id), b.get(id), "entity {id} differs between diff and snapshot");
    }
}

#[test]
fn reconnect_resyncs_identically_and_pin_survives() {
    let lsg = line_world(6, 2.0, "./components/pump.usda");
    let sel = EntityId::from_prim_path("/W/e0");
    // Keep e0 selected so it stays active regardless of the AOI.
    let (_im, mut rsg, mut c) = pipeline(&lsg, [6.0, 0.0, 0.0], 5.0, &[sel]);
    let mut overlay = TwinOverlay::open_in_memory().unwrap();

    let mut server = BridgeServer::new();
    let mut bridge = FakeBridge::new();
    bridge.apply(&[server.hello(&lsg)]);
    let diff = rsg.take_diff(SUB);
    bridge.apply(&server.encode_diff(SUB, &diff, &lsg, &rsg, &overlay).unwrap());
    bridge.hydrate(&mut c).unwrap();

    // Pin e0 through the bridge write-back BEFORE disconnecting.
    let prim = lsg.get(sel).unwrap().prim_path.clone();
    let pinned = Transform::from_translation([0.0, 0.0, 99.0]);
    server
        .handle_pin(sel, &prim, pinned, Some("op"), &mut overlay)
        .unwrap();
    // Re-upsert e0 so the live scene reflects the pin.
    bridge.apply(
        &server
            .encode_diff(
                SUB,
                &vectorflow_sgs::rsg::RsgDiff {
                    upserts: vec![sel],
                    removes: vec![],
                },
                &lsg,
                &rsg,
                &overlay,
            )
            .unwrap(),
    );

    // Capture the live scene, then disconnect (drop the disposable cache).
    let before: Vec<(EntityId, _)> = bridge
        .entity_ids()
        .into_iter()
        .map(|id| (id, bridge.get(id).cloned()))
        .collect();
    bridge.disconnect();
    assert!(bridge.is_empty(), "disconnect drops the cache");
    assert_eq!(bridge.protocol, None);

    // Reconnect: rebuild purely from a fresh Hello + snapshot.
    bridge.apply(&[server.hello(&lsg)]);
    bridge.apply(&server.snapshot(SUB, &lsg, &rsg, &overlay).unwrap());
    bridge.hydrate(&mut c).unwrap();

    let after: Vec<(EntityId, _)> = bridge
        .entity_ids()
        .into_iter()
        .map(|id| (id, bridge.get(id).cloned()))
        .collect();
    assert_eq!(before, after, "reconnect must reconstruct the scene identically");

    // The pin survived the reconnect because it lives in the overlay, not the
    // (discarded) render session.
    assert_eq!(
        bridge.get(sel).unwrap().transform.translation(),
        [0.0, 0.0, 99.0]
    );
}

#[test]
fn pin_and_unpin_write_back_with_zero_usd_or_lsg_mutation() {
    let lsg = line_world(3, 2.0, "./components/pump.usda");
    let (_im, rsg, _c) = pipeline(&lsg, [2.0, 0.0, 0.0], 5.0, &[]);
    let mut overlay = TwinOverlay::open_in_memory().unwrap();
    let start_rev = lsg.revision();
    let start_overlay_rev = overlay.revision().unwrap();

    let server = BridgeServer::new();
    let target = EntityId::from_prim_path("/W/e1");
    let prim = lsg.get(target).unwrap().prim_path.clone();
    let authored = lsg.get(target).unwrap().transform_default;

    // PinPart -> write-back -> PinConfirm.
    let pinned = Transform::from_translation([7.0, 8.0, 9.0]);
    let confirm = server
        .handle_pin(target, &prim, pinned, Some("op"), &mut overlay)
        .unwrap();
    let rev = match confirm {
        BridgeMsg::PinConfirm { id, transform, revision } => {
            assert_eq!(id, target);
            assert_eq!(transform.translation(), [7.0, 8.0, 9.0]);
            revision
        }
        other => panic!("expected PinConfirm, got {other:?}"),
    };
    assert!(rev > start_overlay_rev, "overlay revision must bump");
    assert!(overlay.get_pin(target).unwrap().is_some());

    // The next upsert for the pinned entity carries the pinned transform.
    let mut server = BridgeServer::new();
    let msgs = server
        .encode_diff(
            SUB,
            &vectorflow_sgs::rsg::RsgDiff { upserts: vec![target], removes: vec![] },
            &lsg,
            &rsg,
            &overlay,
        )
        .unwrap();
    match &msgs[0] {
        BridgeMsg::UpsertEntity { transform, .. } => {
            assert_eq!(transform.translation(), [7.0, 8.0, 9.0], "pin > authored");
        }
        other => panic!("expected UpsertEntity, got {other:?}"),
    }

    // UnpinPart -> reverts to authored.
    server.handle_unpin(target, authored, &mut overlay).unwrap();
    assert!(overlay.get_pin(target).unwrap().is_none());
    let msgs = server
        .encode_diff(
            SUB,
            &vectorflow_sgs::rsg::RsgDiff { upserts: vec![target], removes: vec![] },
            &lsg,
            &rsg,
            &overlay,
        )
        .unwrap();
    match &msgs[0] {
        BridgeMsg::UpsertEntity { transform, .. } => {
            assert_eq!(transform.translation(), authored.translation());
        }
        other => panic!("expected UpsertEntity, got {other:?}"),
    }

    // The LSG index was never mutated (proxy for zero USD writes).
    assert_eq!(lsg.revision(), start_rev, "the bridge never mutates the LSG");
}

#[test]
fn coarse_pick_returns_nearest_hit_and_none_on_miss() {
    // Two boxes on the x-axis; a ray down +z at x=0 must hit e0 (nearest).
    let lsg = line_world(3, 10.0, "./components/pump.usda");
    let (_im, rsg, _c) = pipeline(&lsg, [10.0, 0.0, 0.0], 100.0, &[]); // AOI covers all 3
    let server = BridgeServer::new();

    // Ray from behind through e0's column (x=0).
    let hit = server.coarse_pick(SUB, 1, [0.0, 0.0, -50.0], [0.0, 0.0, 1.0], &lsg, &rsg);
    match hit {
        BridgeMsg::PickResult { request_id, hit } => {
            assert_eq!(request_id, 1);
            assert_eq!(hit, Some(EntityId::from_prim_path("/W/e0")));
        }
        other => panic!("expected PickResult, got {other:?}"),
    }

    // Ray far off in +y misses every box.
    let miss = server.coarse_pick(SUB, 2, [0.0, 100.0, -50.0], [0.0, 0.0, 1.0], &lsg, &rsg);
    assert!(matches!(miss, BridgeMsg::PickResult { hit: None, .. }));

    // Direct unit-level sanity on the slab math.
    let box_ = Aabb { min: [-0.5; 3], max: [0.5; 3] };
    assert!(ray_aabb([0.0, 0.0, -5.0], [0.0, 0.0, 1.0], &box_).is_some());
    assert!(ray_aabb([5.0, 0.0, -5.0], [0.0, 0.0, 1.0], &box_).is_none());
}

#[test]
fn geom_refs_carry_uris_that_point_at_fixture_usd() {
    // Reference the three real fixture component files by their fixture-relative
    // URIs; the emitted GeomRefs must resolve to files that exist on disk under
    // assets/usd/pump-station-01 (Python-free: we only check the paths).
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../assets/usd/pump-station-01");
    let components = ["./components/pump.usda", "./components/tank.usda", "./components/distribution_switch.usda"];
    for uri in components {
        assert!(
            fixture.join(uri).exists(),
            "fixture component {uri} should exist under {}",
            fixture.display()
        );
    }

    let lsg = line_world(4, 2.0, components[0]);
    let (_im, mut rsg, _c) = pipeline(&lsg, [4.0, 0.0, 0.0], 100.0, &[]);
    let overlay = TwinOverlay::open_in_memory().unwrap();
    let mut server = BridgeServer::new();
    let diff = rsg.take_diff(SUB);
    let msgs = server.encode_diff(SUB, &diff, &lsg, &rsg, &overlay).unwrap();

    let mut checked = 0;
    for m in &msgs {
        if let BridgeMsg::UpsertEntity { geom_ref: Some(g), .. } = m {
            assert!(
                fixture.join(&g.payload_uri).exists(),
                "emitted GeomRef URI {} must point at a real fixture file",
                g.payload_uri
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "expected at least one GeomRef-bearing upsert");
}

#[test]
fn protocol_is_negotiated_on_connect() {
    assert_eq!(negotiate(&[PROTOCOL_VERSION.to_string()]), Some(PROTOCOL_VERSION));
    assert_eq!(negotiate(&["vf.bridge.v9".to_string()]), None);

    // A Connect handshake with a matching version yields a Hello; the fake
    // bridge records the negotiated protocol.
    let lsg = line_world(1, 2.0, "./components/pump.usda");
    let server = BridgeServer::new();
    let mut bridge = FakeBridge::new();
    bridge.apply(&[server.hello(&lsg)]);
    assert_eq!(bridge.protocol.as_deref(), Some(PROTOCOL_VERSION));

    // RegionWire lowers to the interest Region (bridge builds the same struct).
    let region = RegionWire::Sphere { center: [0.0, 3.0, 0.0], radius: 8.0 };
    assert!(matches!(region.to_region(), Region::Sphere { radius, .. } if radius == 8.0));
}

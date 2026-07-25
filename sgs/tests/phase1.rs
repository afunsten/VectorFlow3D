//! Phase 1 integration tests: LSG projection, stable identity, pin precedence
//! and monotonic revisions, a fixture golden test, and pin persistence across
//! "restarts" (reopening the SQLite Twin Overlay).

use proptest::prelude::*;
use tempfile::tempdir;

use vectorflow_sgs::import::build_from_ndjson;
use vectorflow_sgs::lsg::{EntityId, Transform};
use vectorflow_sgs::overlay::TwinOverlay;

/// A miniature NDJSON stand-in for a payload-deferred facility, so the Rust
/// side is testable without invoking the Python/USD helper.
const FIXTURE_NDJSON: &str = r#"
{"primPath":"/PS","kind":"assembly","vf":{"class":"facility"}}
{"primPath":"/PS/Hall","kind":"group","parent":"/PS","vf":{"zone":"pump_hall"}}
{"primPath":"/PS/Hall/Pump_01","kind":"component","parent":"/PS/Hall","transform":{"translate":[0,0,0]},"extentsHint":[[-1.6,-0.7,-0.7],[0.9,0.7,0.6]],"vf":{"assetTag":"PUMP-01","tags":["duty"]},"geomRef":{"payloadUri":"./components/pump.usda","primPath":"/Pump","contentHash":"deadbeef"},"bindings":[{"attribute":"flow","sourceId":"victoriametrics","query":"pump_flow_gpm{asset=\"PUMP-01\"}","unit":"gpm","ttlMs":5000,"priority":"background","qualityPolicy":"stale_ok"}]}
{"primPath":"/PS/Hall/Pump_02","kind":"component","parent":"/PS/Hall","transform":{"translate":[0,3,0]},"vf":{"assetTag":"PUMP-02"},"geomRef":{"payloadUri":"./components/pump.usda","primPath":"/Pump","contentHash":"deadbeef"},"bindings":[{"attribute":"flow","sourceId":"victoriametrics","query":"pump_flow_gpm{asset=\"PUMP-02\"}"}]}
"#;

#[test]
fn golden_fixture_projection() {
    let lsg = build_from_ndjson(FIXTURE_NDJSON.as_bytes()).unwrap();

    // Hierarchy: 1 assembly + 1 group + 2 components.
    assert_eq!(lsg.len(), 4);
    assert_eq!(lsg.payload_count(), 2);
    assert_eq!(lsg.entities_with_bindings(), 2);
    assert_eq!(lsg.binding_count(), 2);

    // Parent/child links established from the parent references.
    let hall = lsg.by_path("/PS/Hall").unwrap();
    assert_eq!(hall.children.len(), 2);
    assert_eq!(hall.parent, Some(EntityId::from_prim_path("/PS")));

    // Instance-level data indexed without opening payloads.
    let pump = lsg.by_asset_tag("PUMP-01").unwrap();
    assert_eq!(pump.kind.as_deref(), Some("component"));
    assert_eq!(pump.transform_default.translation(), [0.0, 0.0, 0.0]);
    assert_eq!(pump.tags, vec!["duty".to_string()]);
    assert_eq!(pump.extents.min, [-1.6, -0.7, -0.7]);
    let geom = pump.geom_ref.as_ref().unwrap();
    assert_eq!(geom.payload_uri, "./components/pump.usda");
    assert_eq!(geom.content_hash, "deadbeef");

    // Binding index resolves both pumps by the "flow" attribute.
    assert_eq!(lsg.entities_binding("flow").len(), 2);
}

#[test]
fn entity_id_matches_prim_path_hash() {
    let lsg = build_from_ndjson(FIXTURE_NDJSON.as_bytes()).unwrap();
    let pump = lsg.by_asset_tag("PUMP-01").unwrap();
    assert_eq!(pump.id, EntityId::from_prim_path("/PS/Hall/Pump_01"));
    // Selector resolution works by tag and by path to the same entity.
    assert_eq!(
        lsg.resolve_selector("PUMP-01").unwrap().id,
        lsg.resolve_selector("/PS/Hall/Pump_01").unwrap().id
    );
}

#[test]
fn pin_persists_across_overlay_reopen() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("overlay.sqlite");
    let id = EntityId::from_prim_path("/PS/Hall/Pump_01");
    let pinned = Transform::from_translation([1.0, 2.0, 3.0]);

    // "Process 1": write a pin, then drop the connection.
    {
        let mut ov = TwinOverlay::open(&db).unwrap();
        let rev = ov.pin(id, "/PS/Hall/Pump_01", pinned, Some("adam")).unwrap();
        assert_eq!(rev, 1);
    }

    // "Process 2": reopen the same file; the pin survives and wins.
    {
        let ov = TwinOverlay::open(&db).unwrap();
        let pin = ov.get_pin(id).unwrap().expect("pin persisted");
        assert_eq!(pin.transform.translation(), [1.0, 2.0, 3.0]);
        assert_eq!(pin.pinned_by.as_deref(), Some("adam"));
        assert_eq!(ov.pin_count().unwrap(), 1);
        // Revision persisted too (monotonic across restarts).
        assert_eq!(ov.revision().unwrap(), 1);
    }
}

#[test]
fn pin_precedence_over_authored_default() {
    let lsg = build_from_ndjson(FIXTURE_NDJSON.as_bytes()).unwrap();
    let pump = lsg.by_asset_tag("PUMP-02").unwrap(); // authored translate [0,3,0]
    let mut ov = TwinOverlay::open_in_memory().unwrap();

    // No pin -> authored default.
    assert_eq!(ov.resolved_transform(pump).unwrap().translation(), [0.0, 3.0, 0.0]);

    // Pin -> override wins.
    ov.pin(pump.id, &pump.prim_path, Transform::from_translation([7.0, 7.0, 7.0]), None)
        .unwrap();
    assert_eq!(ov.resolved_transform(pump).unwrap().translation(), [7.0, 7.0, 7.0]);
}

proptest! {
    /// EntityId is a deterministic function of prim path: equal paths hash
    /// equal, and (practically) distinct paths hash distinct.
    #[test]
    fn entity_id_is_deterministic(path in "/[A-Za-z0-9_/]{1,40}") {
        let a = EntityId::from_prim_path(&path);
        let b = EntityId::from_prim_path(&path);
        prop_assert_eq!(a, b);
        let other = format!("{path}/x");
        prop_assert_ne!(a, EntityId::from_prim_path(&other));
    }

    /// Overlay revision is strictly monotonic across an arbitrary sequence of
    /// pin/unpin mutations.
    #[test]
    fn overlay_revision_is_monotonic(ops in proptest::collection::vec(any::<bool>(), 1..40)) {
        let mut ov = TwinOverlay::open_in_memory().unwrap();
        let id = EntityId::from_prim_path("/PS/Hall/Pump_01");
        let mut last = ov.revision().unwrap();
        for pin_it in ops {
            if pin_it {
                let rev = ov.pin(id, "/PS/Hall/Pump_01", Transform::identity(), None).unwrap();
                prop_assert!(rev > last);
                last = rev;
            } else {
                let (rev, _existed) = ov.unpin(id).unwrap();
                prop_assert!(rev > last);
                last = rev;
            }
        }
        prop_assert_eq!(ov.revision().unwrap(), last);
    }
}

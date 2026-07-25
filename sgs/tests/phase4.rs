//! Phase 4 integration tests: the Flow3D DSL ↔ Twin-Overlay/LSG path.
//!
//! Proves the spec §7 Phase 4 gates end-to-end (Python-free / VM-free, LSG built
//! from NDJSON):
//! - line/column-accurate, caret-rendered diagnostics for hand-authored source;
//! - stable IDs survive a reload of unchanged source (entity + anchor + edge);
//! - an incremental reload is a **minimal patch** (changing one part touches one
//!   entity);
//! - re-evaluating interest after a reload does **not storm** the RSG
//!   (transitions bounded by the changed entities; the active set is stable);
//! - bindings are stored as declarative bindings, never live values;
//! - vendor USD asset files remain byte-identical (opinions live only in the
//!   Twin Overlay).

use std::collections::HashMap;
use std::path::PathBuf;

use vectorflow_sgs::dsl;
use vectorflow_sgs::import::build_from_ndjson;
use vectorflow_sgs::interest::{InterestManager, Region, Subscription};
use vectorflow_sgs::lsg::{AnchorId, EntityId, Lsg};
use vectorflow_sgs::opinion::{self, Opinion};
use vectorflow_sgs::overlay::TwinOverlay;
use vectorflow_sgs::rsg::Rsg;
use vectorflow_sgs::spatial::SpatialIndex;

/// A tiny pump-station world (assembly + two pumps + one tank) near the origin.
fn pump_world() -> Lsg {
    let nd = r#"
{"primPath":"/PS","kind":"assembly","vf":{"class":"facility"}}
{"primPath":"/PS/Pump_01","kind":"component","parent":"/PS","transform":{"translate":[0,0,0]},"extentsHint":[[-0.5,-0.5,-0.5],[0.5,0.5,0.5]],"vf":{"assetTag":"PUMP-01"}}
{"primPath":"/PS/Pump_02","kind":"component","parent":"/PS","transform":{"translate":[0,3,0]},"extentsHint":[[-0.5,2.5,-0.5],[0.5,3.5,0.5]],"vf":{"assetTag":"PUMP-02"}}
{"primPath":"/PS/Tank_A","kind":"component","parent":"/PS","transform":{"translate":[10,0,0]},"extentsHint":[[9.5,-0.5,-0.5],[10.5,0.5,0.5]],"vf":{"assetTag":"TANK-A"}}
"#;
    build_from_ndjson(nd.as_bytes()).unwrap()
}

const SRC_V1: &str = r#"
scene "PS"
part PUMP-01 {
  tag "duty"
  anchor discharge at (0.9, 0, 0)
  bind efficiency metric("pump_eff{asset=\"PUMP-01\"}") unit "pct"
}
part TANK-A {
  anchor inlet at (0, 0, 4)
}
pipe PUMP-01.discharge -> TANK-A.inlet
"#;

// Same as V1 but PUMP-01's efficiency query changes (one edited opinion).
const SRC_V2: &str = r#"
scene "PS"
part PUMP-01 {
  tag "duty"
  anchor discharge at (0.9, 0, 0)
  bind efficiency metric("pump_eff_v2{asset=\"PUMP-01\"}") unit "pct"
}
part TANK-A {
  anchor inlet at (0, 0, 4)
}
pipe PUMP-01.discharge -> TANK-A.inlet
"#;

fn hashes(ops: &[Opinion]) -> HashMap<String, String> {
    ops.iter().map(|o| (o.key(), o.content_hash())).collect()
}

#[test]
fn diagnostics_have_line_column_and_caret() {
    let lsg = pump_world();
    // `unit` with no following string on line 6.
    let src = "scene \"PS\"\npart PUMP-01 {\n  tag \"duty\"\n  bind flow metric(\"m{asset=\\\"PUMP-01\\\"}\") unit\n}\n";
    let r = dsl::compile(src, &lsg);
    assert!(r.has_errors());
    let err = r.diagnostics.iter().find(|d| d.is_error()).unwrap();
    // The missing `unit` argument is caught at the next token (`}` on line 5).
    assert_eq!(err.span.line, 5, "diag: {err:?}");
    let rendered = dsl::render(err, src, "t.flow3d");
    assert!(rendered.contains(&format!("t.flow3d:{}:{}", err.span.line, err.span.col)), "{rendered}");
    assert!(rendered.contains('^'), "{rendered}");
}

#[test]
fn stable_ids_survive_unchanged_reload() {
    let lsg = pump_world();
    let a = dsl::compile(SRC_V1, &lsg);
    let b = dsl::compile(SRC_V1, &lsg);
    assert!(!a.has_errors() && !b.has_errors());

    // Opinion keys (which embed entity/anchor/edge ids) are identical.
    let ka: Vec<String> = { let mut v: Vec<_> = a.opinions.iter().map(|o| o.key()).collect(); v.sort(); v };
    let kb: Vec<String> = { let mut v: Vec<_> = b.opinions.iter().map(|o| o.key()).collect(); v.sort(); v };
    assert_eq!(ka, kb);

    // The anchor id is a pure function of (entity, name).
    let pump = EntityId::from_prim_path("/PS/Pump_01");
    let want = AnchorId::new(pump, "discharge");
    assert!(a.opinions.iter().any(|o| matches!(o, Opinion::Anchor(an) if an.id == want)));

    // Diff of identical compiles is empty.
    let d = opinion::diff(&b.opinions, &hashes(&a.opinions));
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn incremental_reload_is_a_minimal_patch() {
    let lsg = pump_world();
    let v1 = dsl::compile(SRC_V1, &lsg).opinions;
    let v2 = dsl::compile(SRC_V2, &lsg).opinions;

    let d = opinion::diff(&v2, &hashes(&v1));
    assert_eq!(d.added.len(), 0, "{d:?}");
    assert_eq!(d.removed.len(), 0, "{d:?}");
    assert_eq!(d.changed.len(), 1, "only PUMP-01's binding changed: {d:?}");

    // Exactly one entity is touched by the reload.
    let changed_ops: Vec<Opinion> = v2
        .iter()
        .filter(|o| d.changed.contains(&o.key()))
        .cloned()
        .collect();
    let touched = opinion::touched_entities(&changed_ops);
    assert_eq!(touched.len(), 1);
    assert!(touched.contains(&EntityId::from_prim_path("/PS/Pump_01")));
}

#[test]
fn reload_does_not_storm_the_rsg() {
    let mut lsg = pump_world();
    let v1 = dsl::compile(SRC_V1, &lsg).opinions;
    opinion::reconcile(&mut lsg, &[], &v1);

    // Camera AOI covering both pumps (origin + (0,3,0)), not the far tank.
    let idx = SpatialIndex::build(&lsg);
    let mut im = InterestManager::new();
    im.upsert(Subscription::spatial(
        1,
        Region::Sphere { center: [0.0, 1.5, 0.0], radius: 3.0 },
        1000,
    ))
    .unwrap();
    let mut rsg = Rsg::new(2);
    let mut cache = vectorflow_sgs::hydrate::PayloadCache::new(Box::new(
        vectorflow_sgs::hydrate::StubPayloadLoader,
    ));
    let t0 = im.evaluate(&lsg, &idx);
    rsg.apply(&t0, &lsg, &mut cache, 0).unwrap();
    let active_before = rsg.len();
    assert!(active_before >= 2, "both pumps active");

    // Reload the edited DSL as a minimal in-place patch.
    let v2 = dsl::compile(SRC_V2, &lsg).opinions;
    let touched = opinion::reconcile(&mut lsg, &v1, &v2);
    assert_eq!(touched.len(), 1);

    // Re-evaluate the SAME interest manager: no churn (bindings don't move
    // extents, ids are stable, patch is in place).
    let idx2 = SpatialIndex::build(&lsg);
    let t1 = im.evaluate(&lsg, &idx2);
    assert_eq!(t1.activated.len(), 0, "no re-activation storm");
    assert_eq!(t1.deactivated.len(), 0, "no de-activation storm");
    rsg.apply(&t1, &lsg, &mut cache, 1).unwrap();
    assert_eq!(rsg.len(), active_before, "|RSG| stable across reload");
}

#[test]
fn bindings_are_stored_as_bindings_not_values() {
    let mut lsg = pump_world();
    let v1 = dsl::compile(SRC_V1, &lsg).opinions;
    opinion::reconcile(&mut lsg, &[], &v1);

    let pump = lsg.by_asset_tag("PUMP-01").unwrap();
    let b = pump
        .bindings
        .iter()
        .find(|b| b.attribute == "efficiency")
        .expect("efficiency binding applied to the LSG");
    // A declarative descriptor (how to resolve), never a value.
    assert_eq!(b.query, "pump_eff{asset=\"PUMP-01\"}");
    assert_eq!(b.source_id, "victoriametrics");
    assert_eq!(b.unit, "pct");
    // Serialized opinion carries the query, not a numeric reading.
    let json = serde_json::to_string(&v1[0]).unwrap();
    assert!(!json.contains("\"value\""), "opinions must not carry live values: {json}");
}

#[test]
fn compiling_never_writes_vendor_usd() {
    // Hash the on-disk USD fixture before and after a full compile + persist +
    // reconcile cycle; opinions must land only in the (temp) Twin Overlay.
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../assets/usd/pump-station-01");
    let before = hash_dir_usda(&fixture);
    assert!(!before.is_empty(), "fixture USD files present");

    let mut lsg = pump_world();
    let compiled = dsl::compile(SRC_V1, &lsg);
    assert!(!compiled.has_errors());

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut ov = TwinOverlay::open(tmp.path()).unwrap();
    let (_rev, _diff) = ov
        .apply_opinions("t.flow3d", "h1", &compiled.opinions)
        .unwrap();
    opinion::reconcile(&mut lsg, &[], &compiled.opinions);

    let after = hash_dir_usda(&fixture);
    assert_eq!(before, after, "vendor USD asset layers must remain byte-identical");
}

/// Concatenated (path, len, bytes-hash) of every `.usda` file under `dir`.
fn hash_dir_usda(dir: &std::path::Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    collect_usda(dir, &mut out);
    out.sort();
    out
}

fn collect_usda(dir: &std::path::Path, out: &mut Vec<(String, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_usda(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("usda") {
            let bytes = std::fs::read(&p).unwrap_or_default();
            // Cheap content fingerprint: (name, fletcher-ish sum).
            let sum = bytes.iter().fold(0u64, |a, &b| a.wrapping_mul(31).wrapping_add(b as u64));
            out.push((p.file_name().unwrap().to_string_lossy().to_string(), sum));
        }
    }
}

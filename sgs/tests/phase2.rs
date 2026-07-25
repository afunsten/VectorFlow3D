//! Phase 2 integration tests: Interest Manager + Runtime Scene Graph.
//!
//! End-to-end pipeline (LSG -> SpatialIndex -> InterestManager -> Rsg +
//! PayloadCache), all Python-free via the stub payload loader. Proves the
//! spec §7 Phase 2 gates: a moving AOI keeps |RSG| and open payloads bounded,
//! explicit selection is retained outside the AOI, eviction waits for the grace
//! period, identical payloads hydrate once (content-hash cache), and interest
//! changes never mutate the LSG (a proxy for "no USD writes").

use std::fmt::Write as _;

use vectorflow_sgs::hydrate::{PayloadCache, StubPayloadLoader};
use vectorflow_sgs::import::build_from_ndjson;
use vectorflow_sgs::interest::{InterestManager, Region, Subscription};
use vectorflow_sgs::lsg::{EntityId, Lsg};
use vectorflow_sgs::rsg::Rsg;
use vectorflow_sgs::spatial::SpatialIndex;
use vectorflow_sgs::synth;

/// A line of `n` payload-backed components spaced `spacing` apart along +x.
/// Every component references the same payload (`content_hash`) so the cache
/// can dedupe. Returns NDJSON for `build_from_ndjson`.
fn line_world_ndjson(n: usize, spacing: f64, content_hash: &str) -> String {
    let mut s = String::new();
    for i in 0..n {
        let x = i as f64 * spacing;
        writeln!(
            s,
            r#"{{"primPath":"/W/e{i}","kind":"component","transform":{{"translate":[{x},0,0]}},"extentsHint":[[{lo},-0.5,-0.5],[{hi},0.5,0.5]],"vf":{{"assetTag":"E{i}"}},"geomRef":{{"payloadUri":"./c.usda","primPath":"/C","contentHash":"{content_hash}"}},"bindings":[]}}"#,
            lo = x - 0.5,
            hi = x + 0.5,
        )
        .unwrap();
    }
    s
}

fn line_world(n: usize, spacing: f64, content_hash: &str) -> Lsg {
    build_from_ndjson(line_world_ndjson(n, spacing, content_hash).as_bytes()).unwrap()
}

fn cache() -> PayloadCache {
    PayloadCache::new(Box::new(StubPayloadLoader))
}

#[test]
fn moving_aoi_keeps_rsg_and_payloads_bounded() {
    let n = 60;
    let spacing = 5.0;
    let lsg = line_world(n, spacing, "h1");
    let idx = SpatialIndex::build(&lsg);
    let start_rev = lsg.revision();

    let radius = 7.0;
    let budget = 100; // not binding; AOI itself is the limiter
    let grace = 2u64;
    let mut im = InterestManager::new();
    im.upsert(Subscription::spatial(
        1,
        Region::Sphere { center: [0.0, 0.0, 0.0], radius },
        budget,
    ))
    .unwrap();
    let mut rsg = Rsg::new(grace);
    let mut c = cache();

    let mut max_rsg = 0usize;
    for step in 0..12u64 {
        let center = [step as f64 * 10.0, 0.0, 0.0];
        im.set_region(1, Region::Sphere { center, radius });
        let t = im.evaluate(&lsg, &idx);
        rsg.apply(&t, &lsg, &mut c, step).unwrap();
        rsg.evict_expired(step, &mut c);

        // Budget respected, and the working set stays tiny vs the whole world.
        assert!(im.active_count(1) <= budget);
        assert!(c.loaded_count() <= 1, "only one distinct payload exists");
        max_rsg = max_rsg.max(rsg.len());
    }

    // A radius-7 sphere over 5-unit spacing touches only a handful of entities;
    // with a 2-tick grace the resident set stays far below the 60-entity world.
    assert!(max_rsg < 20, "|RSG| should stay bounded, got {max_rsg}");
    assert_eq!(lsg.revision(), start_rev, "interest must not mutate the LSG");
}

#[test]
fn budget_caps_active_to_nearest() {
    let lsg = line_world(60, 5.0, "h1");
    let idx = SpatialIndex::build(&lsg);
    let mut im = InterestManager::new();
    // A huge AOI covers everything; budget must clamp the active set.
    im.upsert(Subscription::spatial(
        1,
        Region::Sphere { center: [150.0, 0.0, 0.0], radius: 1000.0 },
        5,
    ))
    .unwrap();
    let mut rsg = Rsg::new(1);
    let mut c = cache();
    let t = im.evaluate(&lsg, &idx);
    rsg.apply(&t, &lsg, &mut c, 0).unwrap();
    assert_eq!(im.active_count(1), 5);
    assert_eq!(rsg.len(), 5);
}

#[test]
fn selection_retained_after_aoi_moves_away() {
    let lsg = line_world(30, 5.0, "h1");
    let idx = SpatialIndex::build(&lsg);
    let sel = EntityId::from_prim_path("/W/e0"); // at x=0

    let mut sub = Subscription::spatial(
        1,
        Region::Sphere { center: [0.0, 0.0, 0.0], radius: 6.0 },
        100,
    );
    sub.entity_ids = vec![sel];
    let mut im = InterestManager::new();
    im.upsert(sub).unwrap();
    let mut rsg = Rsg::new(1);
    let mut c = cache();

    // March the AOI far away from e0.
    for step in 0..10u64 {
        let center = [50.0 + step as f64 * 10.0, 0.0, 0.0];
        im.set_region(1, Region::Sphere { center, radius: 6.0 });
        let t = im.evaluate(&lsg, &idx);
        rsg.apply(&t, &lsg, &mut c, step).unwrap();
        rsg.evict_expired(step, &mut c);
    }

    // e0 is nowhere near the AOI but the explicit selection keeps it resident.
    assert!(rsg.contains(sel), "selected entity must survive the AOI leaving");
    assert!(rsg.get(sel).unwrap().subscribers.contains(&1));
}

#[test]
fn eviction_waits_for_grace_and_unloads_payload() {
    let lsg = line_world(4, 5.0, "h1");
    let idx = SpatialIndex::build(&lsg);
    let grace = 3u64;
    let mut im = InterestManager::new();
    im.upsert(Subscription::spatial(
        1,
        Region::Sphere { center: [0.0, 0.0, 0.0], radius: 2.0 },
        100,
    ))
    .unwrap();
    let mut rsg = Rsg::new(grace);
    let mut c = cache();

    // t=0: activate e0.
    let t = im.evaluate(&lsg, &idx);
    rsg.apply(&t, &lsg, &mut c, 0).unwrap();
    assert_eq!(rsg.len(), 1);
    assert_eq!(c.loaded_count(), 1);

    // t=1: move AOI away -> e0 deactivated, scheduled for eviction.
    im.set_region(1, Region::Sphere { center: [1000.0, 0.0, 0.0], radius: 2.0 });
    let t = im.evaluate(&lsg, &idx);
    rsg.apply(&t, &lsg, &mut c, 1).unwrap();
    assert_eq!(rsg.pending_eviction_count(), 1);

    // Before grace expires (deadline = 1 + 3 = 4): still resident + loaded.
    assert!(rsg.evict_expired(3, &mut c).is_empty());
    assert_eq!(rsg.len(), 1);
    assert_eq!(c.loaded_count(), 1);

    // At the deadline: evicted and payload unloaded.
    let evicted = rsg.evict_expired(4, &mut c);
    assert_eq!(evicted, vec![EntityId::from_prim_path("/W/e0")]);
    assert_eq!(rsg.len(), 0);
    assert_eq!(c.loaded_count(), 0);
}

#[test]
fn identical_payloads_hydrate_once() {
    // 40 entities within the AOI, all sharing one payload content hash.
    let lsg = line_world(40, 1.0, "shared-hash");
    let idx = SpatialIndex::build(&lsg);
    let mut im = InterestManager::new();
    im.upsert(Subscription::spatial(
        1,
        Region::Sphere { center: [20.0, 0.0, 0.0], radius: 1000.0 },
        1000,
    ))
    .unwrap();
    let mut rsg = Rsg::new(1);
    let mut c = cache();
    let t = im.evaluate(&lsg, &idx);
    rsg.apply(&t, &lsg, &mut c, 0).unwrap();

    assert_eq!(rsg.len(), 40, "all 40 within the AOI");
    assert_eq!(c.loaded_count(), 1, "one distinct payload open");
    assert_eq!(c.load_calls(), 1, "hydrated once despite 40 instances");
}

#[test]
fn two_subscribers_share_one_page() {
    let lsg = line_world(10, 5.0, "h1");
    let idx = SpatialIndex::build(&lsg);
    let e0 = EntityId::from_prim_path("/W/e0");
    let mut im = InterestManager::new();
    im.upsert(Subscription::spatial(1, Region::Sphere { center: [0.0; 3], radius: 2.0 }, 100)).unwrap();
    im.upsert(Subscription::spatial(2, Region::Sphere { center: [0.0; 3], radius: 2.0 }, 100)).unwrap();
    let mut rsg = Rsg::new(1);
    let mut c = cache();
    let t = im.evaluate(&lsg, &idx);
    rsg.apply(&t, &lsg, &mut c, 0).unwrap();

    // One shared page, referenced by both subscriptions.
    assert_eq!(rsg.get(e0).unwrap().subscribers.len(), 2);
    assert_eq!(c.load_calls(), 1);

    // Draining per-subscriber diffs: each sees e0 enter its own view.
    assert!(rsg.take_diff(1).upserts.contains(&e0));
    assert!(rsg.take_diff(2).upserts.contains(&e0));
}

#[test]
fn broad_phase_narrows_candidates_at_scale() {
    // Synthetic 200k-entity world; a tight AOI must yield candidates far below
    // the total (toward the §6.5 "50k candidates" envelope, not a full scan).
    let lsg = synth::generate(200_000);
    let idx = SpatialIndex::build(&lsg);
    let cands = idx.candidates_for_sphere([25.0, 25.0, 25.0], 10.0);
    assert!(
        cands.len() < 2_000,
        "broad-phase should narrow 200k -> a small candidate set, got {}",
        cands.len()
    );
    assert!(!cands.is_empty(), "AOI should find some candidates");
}

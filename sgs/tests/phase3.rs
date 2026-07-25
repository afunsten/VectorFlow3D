//! Phase 3 integration tests: lazy telemetry resolvers.
//!
//! End-to-end pipeline (LSG -> SpatialIndex -> InterestManager -> Rsg ->
//! resolve_active), Python-free and VM-free via the stub resolver. Proves the
//! spec §7 Phase 3 gates: inactive bindings cause zero upstream traffic;
//! activating ~1k entities batches by metric; TTL serves the RSG cache then
//! refreshes; a resolver outage keeps prior values and downgrades them to
//! stale; high-priority bindings resolve before background ones; and resolution
//! never mutates the LSG (a proxy for "no live values in USD").

use std::fmt::Write as _;

use vectorflow_sgs::import::build_from_ndjson;
use vectorflow_sgs::interest::{InterestManager, Region, Subscription};
use vectorflow_sgs::lsg::{EntityId, Lsg};
use vectorflow_sgs::resolver::{
    resolve_active, Priority, Quality, ResolveRequest, ResolvedSample, Resolver, StubResolver,
};
use vectorflow_sgs::rsg::Rsg;
use vectorflow_sgs::spatial::SpatialIndex;

/// `n` payload-free components on a line near the origin, each binding one of a
/// few metrics with a convention-shaped PromQL query (`metric{asset="E<i>"}`).
fn metric_world_ndjson(n: usize) -> String {
    let metrics = ["flow", "level", "load"];
    let mut s = String::new();
    for i in 0..n {
        let x = i as f64 * 0.5;
        let m = metrics[i % metrics.len()];
        writeln!(
            s,
            r#"{{"primPath":"/W/e{i}","kind":"component","transform":{{"translate":[{x},0,0]}},"extentsHint":[[{lo},-0.1,-0.1],[{hi},0.1,0.1]],"vf":{{"assetTag":"E{i}"}},"bindings":[{{"attribute":"{m}","sourceId":"victoriametrics","query":"{m}{{asset=\"E{i}\"}}","ttlMs":5000,"priority":"background","qualityPolicy":"stale_ok"}}]}}"#,
            lo = x - 0.1,
            hi = x + 0.1,
        )
        .unwrap();
    }
    s
}

fn metric_world(n: usize) -> Lsg {
    build_from_ndjson(metric_world_ndjson(n).as_bytes()).unwrap()
}

/// Activate every entity in `lsg` under one selection-only subscription (an
/// explicit id list bypasses the spatial region, spec §3.2 — no broad-phase
/// enumeration over a huge radius).
fn activate_all(lsg: &Lsg, budget: usize) -> Rsg {
    let idx = SpatialIndex::build(lsg);
    let ids: Vec<EntityId> = lsg.entities().map(|e| e.id).collect();
    let mut sub = Subscription::spatial(
        1,
        Region::Sphere { center: [0.0, 0.0, 0.0], radius: 1.0 },
        budget,
    );
    sub.region = None;
    sub.entity_ids = ids;
    let mut im = InterestManager::new();
    im.upsert(sub).unwrap();
    let mut rsg = Rsg::new(1);
    let mut cache = vectorflow_sgs::hydrate::PayloadCache::new(Box::new(
        vectorflow_sgs::hydrate::StubPayloadLoader,
    ));
    let t = im.evaluate(lsg, &idx);
    rsg.apply(&t, lsg, &mut cache, 0).unwrap();
    rsg
}

#[test]
fn inactive_bindings_cause_zero_vm_traffic() {
    let lsg = metric_world(50);
    // An empty RSG (nothing activated): resolution must not touch the resolver.
    let mut rsg = Rsg::new(1);
    let mut resolver = StubResolver::new(1.0);
    let stats = resolve_active(&mut rsg, &lsg, &mut resolver, 1000);

    assert_eq!(stats.requests_issued, 0);
    assert_eq!(resolver.upstream_requests(), 0, "no active entity => no VM traffic");
    assert_eq!(resolver.batch_calls(), 0, "resolve() not even invoked");
}

#[test]
fn activating_1k_entities_batches_by_metric() {
    let lsg = metric_world(1000);
    let mut rsg = activate_all(&lsg, 2000);
    assert_eq!(rsg.len(), 1000);

    let mut resolver = StubResolver::new(2.5);
    let stats = resolve_active(&mut rsg, &lsg, &mut resolver, 1000);

    // 1000 bindings resolved, but collapsed to one upstream query per metric.
    assert_eq!(stats.requests_issued, 1000);
    assert_eq!(stats.ok, 1000);
    assert_eq!(
        resolver.upstream_requests(),
        3,
        "1000 bindings over 3 metrics => 3 batched queries"
    );
}

#[test]
fn ttl_serves_cache_then_refreshes() {
    let lsg = metric_world(10);
    let mut rsg = activate_all(&lsg, 100);
    let mut resolver = StubResolver::new(1.0);

    // First pass at t=1000: all fetched.
    let s0 = resolve_active(&mut rsg, &lsg, &mut resolver, 1000);
    assert_eq!(s0.requests_issued, 10);
    assert_eq!(s0.cache_hits, 0);
    let upstream_after_first = resolver.upstream_requests();

    // Within TTL (5000ms): everything served from the RSG cache, no new fetch.
    let s1 = resolve_active(&mut rsg, &lsg, &mut resolver, 2000);
    assert_eq!(s1.requests_issued, 0);
    assert_eq!(s1.cache_hits, 10);
    assert_eq!(resolver.upstream_requests(), upstream_after_first, "cache hit => no VM traffic");

    // Past TTL (as_of 1000 + ttl 5000 = 6000): refresh.
    let s2 = resolve_active(&mut rsg, &lsg, &mut resolver, 6001);
    assert_eq!(s2.requests_issued, 10);
    assert!(resolver.upstream_requests() > upstream_after_first, "expired => refetch");
}

#[test]
fn outage_downgrades_prior_value_to_stale() {
    let lsg = metric_world(6);
    let mut rsg = activate_all(&lsg, 100);

    // Good pass first: real values cached.
    let mut good = StubResolver::new(7.0);
    let s0 = resolve_active(&mut rsg, &lsg, &mut good, 1000);
    assert_eq!(s0.ok, 6);

    // Past TTL, resolver is down: prior values stay visible but go stale.
    let mut down = StubResolver::outage();
    let s1 = resolve_active(&mut rsg, &lsg, &mut down, 6001);
    assert_eq!(s1.stale, 6);
    assert_eq!(s1.ok, 0);
    assert_eq!(s1.unavailable, 0);

    for re in rsg.entities() {
        for v in re.telemetry.values() {
            assert_eq!(v.value, 7.0, "stale value is the last known good");
            assert_eq!(v.quality, Quality::Stale);
        }
    }
}

/// Records the order and priority of requests as the resolver receives them.
#[derive(Default)]
struct RecordingResolver {
    seen: Vec<Priority>,
    upstream: u64,
    batch: u64,
}

impl Resolver for RecordingResolver {
    fn resolve(&mut self, reqs: &[ResolveRequest]) -> Vec<ResolvedSample> {
        self.batch += 1;
        let mut out = Vec::with_capacity(reqs.len());
        for r in reqs {
            self.seen.push(r.priority);
            self.upstream += 1;
            out.push(ResolvedSample {
                entity_id: r.entity_id,
                attribute: r.attribute.clone(),
                value: 1.0,
                quality: Quality::Ok,
            });
        }
        out
    }
    fn upstream_requests(&self) -> u64 {
        self.upstream
    }
    fn batch_calls(&self) -> u64 {
        self.batch
    }
}

#[test]
fn priority_orders_high_before_background() {
    // Four entities, alternating high / background priority bindings.
    let mut nd = String::new();
    for i in 0..4 {
        let prio = if i % 2 == 0 { "high" } else { "background" };
        writeln!(
            nd,
            r#"{{"primPath":"/W/e{i}","kind":"component","transform":{{"translate":[{i},0,0]}},"extentsHint":[[{lo},-0.1,-0.1],[{hi},0.1,0.1]],"vf":{{"assetTag":"E{i}"}},"bindings":[{{"attribute":"a{i}","sourceId":"victoriametrics","query":"m{i}{{asset=\"E{i}\"}}","ttlMs":5000,"priority":"{prio}","qualityPolicy":"stale_ok"}}]}}"#,
            lo = i as f64 - 0.1,
            hi = i as f64 + 0.1,
        )
        .unwrap();
    }
    let lsg = build_from_ndjson(nd.as_bytes()).unwrap();
    let mut rsg = activate_all(&lsg, 100);

    let mut resolver = RecordingResolver::default();
    resolve_active(&mut rsg, &lsg, &mut resolver, 1000);

    // Once a background request appears, no high-priority request may follow.
    let mut seen_background = false;
    for p in &resolver.seen {
        match p {
            Priority::Background => seen_background = true,
            Priority::High => assert!(!seen_background, "high must precede background"),
        }
    }
    assert_eq!(resolver.seen.len(), 4);
}

#[test]
fn resolution_never_mutates_lsg() {
    let lsg = metric_world(20);
    let start_rev = lsg.revision();
    let mut rsg = activate_all(&lsg, 100);
    let mut resolver = StubResolver::new(5.0);
    resolve_active(&mut rsg, &lsg, &mut resolver, 1000);
    assert_eq!(lsg.revision(), start_rev, "resolution must not mutate the LSG (no live values in USD)");
}

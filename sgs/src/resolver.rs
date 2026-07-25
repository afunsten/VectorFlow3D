//! Telemetry resolvers (spec §3.4, Phase 3): lazy, **resolve-on-activate**.
//!
//! A resolver turns declarative [`TelemetryBinding`] descriptors into values —
//! only for entities that some subscription has activated into the Runtime
//! Scene Graph. Nothing here is on the LSG/USD path: inactive bindings cause
//! zero upstream traffic, and resolved values live in the RSG cache, never in
//! USD (spec hard rule).
//!
//! The [`Resolver`] trait is the seam a real backend or a test stub plug into
//! (mirroring [`crate::hydrate::PayloadLoader`]). The day-one metrics backend
//! is [`VictoriaMetricsResolver`], a thin blocking PromQL client (`ureq`, no
//! async runtime) that **batches by metric**: requests for the same metric are
//! collapsed into a single `metric{asset=~"A|B|C"}` instant query and demuxed
//! back to entities by the `asset` label (the label the pre-v1 convention binds
//! to `vf.assetTag`, spec §4.2/§4.7). The resolver is the ONLY component that
//! speaks PromQL — O3DE and the bridge never do.
//!
//! Caveat (locked): the trait insulates against swapping the HTTP client, but
//! not against a future sync→async transition of `resolve` itself — that is a
//! real signature change, only warranted if concurrent resolver calls are
//! actually needed (multi-node SGS / large fan-out).

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::lsg::{EntityId, Lsg};
use crate::rsg::Rsg;

/// Quality flag on a resolved value (spec §3.4 responsibility 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// Freshly resolved from upstream.
    Ok,
    /// Prior cached value served because a refresh failed (stale-while-revalidate).
    Stale,
    /// No value available (query matched nothing) and no prior value cached.
    Unavailable,
    /// The binding could not be resolved (bad source / parse error).
    Error,
}

impl Quality {
    pub fn as_str(self) -> &'static str {
        match self {
            Quality::Ok => "ok",
            Quality::Stale => "stale",
            Quality::Unavailable => "unavailable",
            Quality::Error => "error",
        }
    }
}

/// A resolved telemetry sample held in the RSG (spec §4.4 `telemetry` map).
/// This IS the cache entry: freshness is `as_of + ttl`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TelemetryValue {
    pub value: f64,
    /// Wall-clock time (ms since epoch) the value was resolved / last touched.
    pub as_of_ms: i64,
    /// TTL from the binding descriptor.
    pub ttl_ms: f64,
    pub quality: Quality,
}

impl TelemetryValue {
    /// Still fresh at `now_ms` (within `as_of + ttl`). Unavailable/Error entries
    /// are never considered fresh so they are retried.
    pub fn is_fresh(&self, now_ms: i64) -> bool {
        if matches!(self.quality, Quality::Unavailable | Quality::Error) {
            return false;
        }
        self.as_of_ms.saturating_add(self.ttl_ms as i64) > now_ms
    }
}

/// Resolver priority projected from a binding's `priority` string (spec §3.4
/// responsibility 4: selection/alerts high, camera-AOI background).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    High,
    Background,
}

impl Priority {
    pub fn from_binding(s: &str) -> Self {
        if s.eq_ignore_ascii_case("high") {
            Priority::High
        } else {
            Priority::Background
        }
    }

    fn rank(self) -> u8 {
        match self {
            Priority::High => 0,
            Priority::Background => 1,
        }
    }
}

/// One unit of work handed to a [`Resolver`]: which entity + attribute, the
/// PromQL query, its TTL, and its priority. Owns its strings so the trait is
/// free of lifetimes (see the sync→async caveat above).
#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub entity_id: EntityId,
    pub attribute: String,
    pub source_id: String,
    pub query: String,
    pub ttl_ms: f64,
    pub priority: Priority,
}

/// A resolver's answer for one request. `Stale` is never produced here — it is
/// applied by [`resolve_active`] when a failed refresh falls back to a prior
/// cached value.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSample {
    pub entity_id: EntityId,
    pub attribute: String,
    pub value: f64,
    pub quality: Quality,
}

impl ResolvedSample {
    fn ok(entity_id: EntityId, attribute: String, value: f64) -> Self {
        ResolvedSample { entity_id, attribute, value, quality: Quality::Ok }
    }

    fn quality(entity_id: EntityId, attribute: String, quality: Quality) -> Self {
        ResolvedSample { entity_id, attribute, value: f64::NAN, quality }
    }
}

/// Strategy for turning declarative binding descriptors into values.
pub trait Resolver {
    /// Resolve a batch of requests, returning one sample per request.
    fn resolve(&mut self, reqs: &[ResolveRequest]) -> Vec<ResolvedSample>;
    /// Upstream round-trips issued so far (VM HTTP calls). The metric to prove
    /// "inactive bindings cause zero VM traffic" and batching.
    fn upstream_requests(&self) -> u64;
    /// Number of `resolve` batch invocations so far.
    fn batch_calls(&self) -> u64;
}

// ---------------------------------------------------------------------------
// Query parsing + batch planning (shared by every resolver so batch accounting
// is identical online and offline).
// ---------------------------------------------------------------------------

/// A `metric{...,asset="TAG",...}` PromQL query decomposed for batching.
#[derive(Debug, Clone, PartialEq)]
struct ParsedQuery {
    metric: String,
    asset: String,
    /// Label matchers other than `asset`, preserved verbatim.
    others: Vec<String>,
}

/// Parse the locked convention shape `metric{asset="TAG"[, other="..."]}`.
/// Returns `None` for anything that does not fit (those fall back to being
/// issued individually, unbatched).
fn parse_query(query: &str) -> Option<ParsedQuery> {
    let q = query.trim();
    let open = q.find('{')?;
    let close = q.rfind('}')?;
    if close < open {
        return None;
    }
    let metric = q[..open].trim().to_string();
    if metric.is_empty() {
        return None;
    }
    let inner = &q[open + 1..close];
    let mut asset = None;
    let mut others = Vec::new();
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(rest) = part.strip_prefix("asset") {
            // rest begins with the operator (`=`, `=~`, ...) then a quoted value.
            asset = Some(extract_quoted(rest)?);
        } else {
            others.push(part.to_string());
        }
    }
    Some(ParsedQuery {
        metric,
        asset: asset?,
        others,
    })
}

/// Extract the first double-quoted substring's contents.
fn extract_quoted(s: &str) -> Option<String> {
    let a = s.find('"')?;
    let rest = &s[a + 1..];
    let b = rest.find('"')?;
    Some(rest[..b].to_string())
}

/// Escape RE2 metacharacters so an asset tag is matched literally inside an
/// `asset=~"..."` alternation.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Build a batched instant query `metric{[others,] asset=~"A|B|C"}`.
fn build_batched_query(metric: &str, others: &[String], assets: &[String]) -> String {
    let alt = assets
        .iter()
        .map(|a| escape_regex(a))
        .collect::<Vec<_>>()
        .join("|");
    let mut matchers = others.to_vec();
    matchers.push(format!("asset=~\"{alt}\""));
    format!("{metric}{{{}}}", matchers.join(","))
}

/// A single upstream query plus the requests it satisfies. Each member records
/// the `asset` label used to demux the response (or `None` when the query is
/// issued verbatim and there is nothing to demux).
struct Batch {
    query: String,
    members: Vec<(usize, Option<String>)>,
}

/// Maximum number of `asset` alternatives packed into a single
/// `asset=~"A|B|C"` query. Metric batching is proven at camera-AOI scale
/// (~hundreds of active entities), but a persistent automation rule such as
/// "all pumps in alarm" (spec §3.2) can force-activate far more entities at
/// once and would otherwise collapse into a single, unboundedly-large regex
/// query / URL against VictoriaMetrics. Splitting an over-cap group into
/// `ceil(N / cap)` bounded queries keeps each request a sane size while still
/// collapsing the common case to one query. Tune per deployment (VM/proxy URL
/// and regex limits); 512 is a conservative default that keeps AOI- and
/// moderate-automation-scale activations to one query per metric while still
/// splitting pathological (thousands-of-asset) rule activations.
///
/// Latency note: `resolve` is synchronous (`ureq`), so a group that splits into
/// `N` chunks costs `N×` wall-clock latency for that pass. That is the concrete
/// trigger for the deferred sync→async transition (see the module caveat):
/// **routine** chunking under real automation-rule load is the signal to add
/// concurrent resolver calls — not a speculative future need.
const MAX_ASSETS_PER_QUERY: usize = 512;

/// Collapse requests into a small set of upstream queries: group PromQL
/// requests that share a metric (and non-asset matchers) into one
/// `asset=~"..."` query, split so no query exceeds [`MAX_ASSETS_PER_QUERY`]
/// assets; everything else is issued individually.
fn plan_batches(reqs: &[ResolveRequest]) -> Vec<Batch> {
    /// (metric, sorted non-asset matchers, members as `(request index, asset)`).
    type Group = (String, Vec<String>, Vec<(usize, String)>);
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    let mut ungroupable: Vec<usize> = Vec::new();

    for (i, r) in reqs.iter().enumerate() {
        if r.source_id != "victoriametrics" {
            ungroupable.push(i);
            continue;
        }
        match parse_query(&r.query) {
            Some(p) => {
                let mut others = p.others;
                others.sort();
                let key = format!("{}\u{1}{}", p.metric, others.join(","));
                let entry = groups
                    .entry(key)
                    .or_insert_with(|| (p.metric.clone(), others.clone(), Vec::new()));
                entry.2.push((i, p.asset));
            }
            None => ungroupable.push(i),
        }
    }

    let mut batches = Vec::new();
    for (_key, (metric, others, members)) in groups {
        // Split an over-cap group so no single query carries more than
        // MAX_ASSETS_PER_QUERY asset alternatives.
        for chunk in members.chunks(MAX_ASSETS_PER_QUERY) {
            let mut assets: Vec<String> = chunk.iter().map(|(_, a)| a.clone()).collect();
            assets.sort();
            assets.dedup();
            let query = build_batched_query(&metric, &others, &assets);
            let members = chunk.iter().map(|(i, a)| (*i, Some(a.clone()))).collect();
            batches.push(Batch { query, members });
        }
    }
    for i in ungroupable {
        batches.push(Batch {
            query: reqs[i].query.clone(),
            members: vec![(i, None)],
        });
    }
    batches
}

// ---------------------------------------------------------------------------
// VictoriaMetrics PromQL resolver
// ---------------------------------------------------------------------------

/// Blocking VictoriaMetrics PromQL adapter (spec §3.4 locked backend).
pub struct VictoriaMetricsResolver {
    base_url: String,
    agent: ureq::Agent,
    upstream_requests: u64,
    batch_calls: u64,
}

impl VictoriaMetricsResolver {
    pub fn new(base_url: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(10))
            .build();
        VictoriaMetricsResolver {
            base_url: base_url.into(),
            agent,
            upstream_requests: 0,
            batch_calls: 0,
        }
    }

    /// Issue one instant query and return `(asset_label, value)` rows.
    fn query_vm(&self, promql: &str) -> Result<Vec<(Option<String>, f64)>> {
        let url = format!("{}/api/v1/query", self.base_url.trim_end_matches('/'));
        let resp = self
            .agent
            .get(&url)
            .query("query", promql)
            .call()
            .map_err(|e| anyhow!("VM query failed: {e}"))?;
        let body = resp.into_string().context("reading VM response body")?;
        parse_vm_vector(&body)
    }
}

impl Resolver for VictoriaMetricsResolver {
    fn resolve(&mut self, reqs: &[ResolveRequest]) -> Vec<ResolvedSample> {
        self.batch_calls += 1;
        let batches = plan_batches(reqs);
        let mut out = Vec::with_capacity(reqs.len());
        for batch in &batches {
            self.upstream_requests += 1;
            match self.query_vm(&batch.query) {
                Ok(rows) => {
                    let mut by_asset: HashMap<String, f64> = HashMap::new();
                    let mut first = None;
                    for (asset, v) in &rows {
                        if let Some(a) = asset {
                            by_asset.entry(a.clone()).or_insert(*v);
                        }
                        if first.is_none() {
                            first = Some(*v);
                        }
                    }
                    for (idx, asset) in &batch.members {
                        let r = &reqs[*idx];
                        let val = match asset {
                            Some(a) => by_asset.get(a).copied(),
                            None => first,
                        };
                        out.push(match val {
                            Some(v) => ResolvedSample::ok(r.entity_id, r.attribute.clone(), v),
                            None => ResolvedSample::quality(
                                r.entity_id,
                                r.attribute.clone(),
                                Quality::Unavailable,
                            ),
                        });
                    }
                }
                Err(_) => {
                    for (idx, _) in &batch.members {
                        let r = &reqs[*idx];
                        out.push(ResolvedSample::quality(
                            r.entity_id,
                            r.attribute.clone(),
                            Quality::Unavailable,
                        ));
                    }
                }
            }
        }
        out
    }

    fn upstream_requests(&self) -> u64 {
        self.upstream_requests
    }

    fn batch_calls(&self) -> u64 {
        self.batch_calls
    }
}

/// VM `/api/v1/query` response envelope (instant vector).
#[derive(Debug, Deserialize)]
struct VmResponse {
    status: String,
    #[serde(default)]
    data: Option<VmData>,
}

#[derive(Debug, Deserialize)]
struct VmData {
    #[serde(default)]
    result: Vec<VmSeries>,
}

#[derive(Debug, Deserialize)]
struct VmSeries {
    #[serde(default)]
    metric: HashMap<String, String>,
    /// `[<unix_ts_float>, "<value_string>"]`.
    #[serde(default)]
    value: Option<(f64, String)>,
}

/// Parse a VM instant-vector response into `(asset_label, value)` rows. Pure /
/// no I/O so it is a contract test target (spec §6.4) without a live VM.
pub fn parse_vm_vector(json: &str) -> Result<Vec<(Option<String>, f64)>> {
    let resp: VmResponse = serde_json::from_str(json).context("parsing VM JSON")?;
    if resp.status != "success" {
        bail!("VM query status: {}", resp.status);
    }
    let data = resp.data.ok_or_else(|| anyhow!("VM response missing data"))?;
    let mut out = Vec::new();
    for s in data.result {
        if let Some((_, ref vs)) = s.value {
            if let Ok(v) = vs.parse::<f64>() {
                out.push((s.metric.get("asset").cloned(), v));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Stub resolver (offline / tests)
// ---------------------------------------------------------------------------

/// Resolver for offline demos and Python/VM-free tests. Returns a canned value
/// for every request (or simulates an outage when `value` is `None`). Uses the
/// same [`plan_batches`] accounting as the real resolver so batching can be
/// proven without a network.
pub struct StubResolver {
    /// `Some(v)` => every request resolves to `v` (`Ok`); `None` => outage
    /// (`Unavailable`), exercising the stale-while-revalidate fallback.
    pub value: Option<f64>,
    upstream_requests: u64,
    batch_calls: u64,
}

impl StubResolver {
    pub fn new(value: f64) -> Self {
        StubResolver { value: Some(value), upstream_requests: 0, batch_calls: 0 }
    }

    /// A stub that always fails (simulates a VM outage).
    pub fn outage() -> Self {
        StubResolver { value: None, upstream_requests: 0, batch_calls: 0 }
    }
}

impl Resolver for StubResolver {
    fn resolve(&mut self, reqs: &[ResolveRequest]) -> Vec<ResolvedSample> {
        self.batch_calls += 1;
        let batches = plan_batches(reqs);
        let mut out = Vec::with_capacity(reqs.len());
        for batch in &batches {
            self.upstream_requests += 1;
            for (idx, _asset) in &batch.members {
                let r = &reqs[*idx];
                out.push(match self.value {
                    Some(v) => ResolvedSample::ok(r.entity_id, r.attribute.clone(), v),
                    None => ResolvedSample::quality(
                        r.entity_id,
                        r.attribute.clone(),
                        Quality::Unavailable,
                    ),
                });
            }
        }
        out
    }

    fn upstream_requests(&self) -> u64 {
        self.upstream_requests
    }

    fn batch_calls(&self) -> u64 {
        self.batch_calls
    }
}

// ---------------------------------------------------------------------------
// The lazy resolve pass over the RSG
// ---------------------------------------------------------------------------

/// Observability counters from one [`resolve_active`] pass (spec §6.3).
#[derive(Debug, Default, Clone, Copy)]
pub struct ResolveStats {
    /// Bindings that needed a fetch (missing or expired in the RSG cache).
    pub requests_issued: usize,
    /// Bindings served from a still-fresh RSG cache entry (no fetch).
    pub cache_hits: usize,
    /// Upstream round-trips this pass added (VM HTTP calls).
    pub upstream_delta: u64,
    pub ok: usize,
    pub stale: usize,
    pub unavailable: usize,
}

impl ResolveStats {
    /// Cache hit ratio over the bindings considered this pass.
    pub fn hit_ratio(&self) -> f64 {
        let total = self.requests_issued + self.cache_hits;
        if total == 0 {
            1.0
        } else {
            self.cache_hits as f64 / total as f64
        }
    }
}

/// Resolve telemetry for the active working set (spec §3.4 resolve-on-activate).
///
/// Only entities materialized in the RSG are considered, so inactive bindings
/// never touch the resolver. For each active entity's bindings we skip
/// still-fresh cache entries (TTL), batch the rest **high-priority first**, call
/// the resolver once, and write results back into each entity's RSG telemetry
/// map. A failed refresh keeps the prior value and downgrades it to `Stale`
/// (stale-while-revalidate). This reads the LSG and writes ONLY the RSG — never
/// USD, the LSG, or the Twin Overlay.
pub fn resolve_active(
    rsg: &mut Rsg,
    lsg: &Lsg,
    resolver: &mut dyn Resolver,
    now_ms: i64,
) -> ResolveStats {
    let mut stats = ResolveStats::default();
    let mut reqs: Vec<ResolveRequest> = Vec::new();

    for re in rsg.entities() {
        let Some(e) = lsg.get(re.id) else { continue };
        for b in &e.bindings {
            match re.telemetry.get(&b.attribute) {
                Some(tv) if tv.is_fresh(now_ms) => stats.cache_hits += 1,
                _ => reqs.push(ResolveRequest {
                    entity_id: re.id,
                    attribute: b.attribute.clone(),
                    source_id: b.source_id.clone(),
                    query: b.query.clone(),
                    ttl_ms: b.ttl_ms,
                    priority: Priority::from_binding(&b.priority),
                }),
            }
        }
    }

    stats.requests_issued = reqs.len();
    if reqs.is_empty() {
        return stats;
    }

    // High-priority bindings first (stable within a priority, spec §3.4 #4).
    reqs.sort_by_key(|r| r.priority.rank());

    // TTL lookup for stamping results.
    let ttl_of: HashMap<(EntityId, String), f64> = reqs
        .iter()
        .map(|r| ((r.entity_id, r.attribute.clone()), r.ttl_ms))
        .collect();

    let before = resolver.upstream_requests();
    let results = resolver.resolve(&reqs);
    stats.upstream_delta = resolver.upstream_requests().saturating_sub(before);

    for res in results {
        let ttl_ms = ttl_of
            .get(&(res.entity_id, res.attribute.clone()))
            .copied()
            .unwrap_or(0.0);
        let Some(re) = rsg.get_mut(res.entity_id) else { continue };
        match res.quality {
            Quality::Ok => {
                re.telemetry.insert(
                    res.attribute,
                    TelemetryValue {
                        value: res.value,
                        as_of_ms: now_ms,
                        ttl_ms,
                        quality: Quality::Ok,
                    },
                );
                stats.ok += 1;
            }
            Quality::Unavailable | Quality::Error => {
                match re.telemetry.get_mut(&res.attribute) {
                    // Prior real value: keep it visible, downgrade to stale
                    // (stale-while-revalidate).
                    Some(prev) if matches!(prev.quality, Quality::Ok | Quality::Stale) => {
                        prev.quality = Quality::Stale;
                        stats.stale += 1;
                    }
                    // Prior was itself a failure: still no real value; record
                    // the retry timestamp but stay unavailable.
                    Some(prev) => {
                        prev.as_of_ms = now_ms;
                        prev.quality = res.quality;
                        stats.unavailable += 1;
                    }
                    None => {
                        re.telemetry.insert(
                            res.attribute,
                            TelemetryValue {
                                value: f64::NAN,
                                as_of_ms: now_ms,
                                ttl_ms,
                                quality: res.quality,
                            },
                        );
                        stats.unavailable += 1;
                    }
                }
            }
            Quality::Stale => stats.stale += 1,
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metric_and_asset() {
        let p = parse_query("pump_flow_gpm{asset=\"PUMP-01\"}").unwrap();
        assert_eq!(p.metric, "pump_flow_gpm");
        assert_eq!(p.asset, "PUMP-01");
        assert!(p.others.is_empty());
    }

    #[test]
    fn parses_query_with_extra_labels() {
        let p = parse_query("m{zone=\"a\",asset=\"X-1\",unit=\"psi\"}").unwrap();
        assert_eq!(p.metric, "m");
        assert_eq!(p.asset, "X-1");
        assert_eq!(p.others, vec!["zone=\"a\"".to_string(), "unit=\"psi\"".to_string()]);
    }

    #[test]
    fn unparseable_queries_return_none() {
        assert!(parse_query("scalar_expr").is_none());
        assert!(parse_query("m{no_asset=\"x\"}").is_none());
    }

    #[test]
    fn batched_query_builds_regex_alternation() {
        let q = build_batched_query("pump_flow_gpm", &[], &["PUMP-01".into(), "PUMP-02".into()]);
        assert_eq!(q, "pump_flow_gpm{asset=~\"PUMP-01|PUMP-02\"}");
    }

    #[test]
    fn regex_escapes_metacharacters() {
        assert_eq!(escape_regex("A.B+C"), "A\\.B\\+C");
    }

    #[test]
    fn plan_batches_groups_by_metric() {
        let reqs = vec![
            req("A", "pump_flow_gpm{asset=\"PUMP-01\"}"),
            req("B", "pump_flow_gpm{asset=\"PUMP-02\"}"),
            req("C", "tank_level_pct{asset=\"TANK-A\"}"),
        ];
        let batches = plan_batches(&reqs);
        // Two metrics -> two upstream queries despite three requests.
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn plan_batches_caps_alternation_width() {
        // One metric over many assets (an automation-rule-scale activation)
        // splits into ceil(N / MAX_ASSETS_PER_QUERY) bounded queries.
        let n = MAX_ASSETS_PER_QUERY * 2 + 5;
        let reqs: Vec<ResolveRequest> = (0..n)
            .map(|i| {
                req(
                    &format!("/W/e{i}"),
                    &format!("pump_flow_gpm{{asset=\"E{i}\"}}"),
                )
            })
            .collect();
        let batches = plan_batches(&reqs);
        let expected = n.div_ceil(MAX_ASSETS_PER_QUERY);
        assert_eq!(batches.len(), expected, "over-cap group must split");
        // Every member is still accounted for exactly once (demux intact).
        let total_members: usize = batches.iter().map(|b| b.members.len()).sum();
        assert_eq!(total_members, n);
        for b in &batches {
            assert!(b.members.len() <= MAX_ASSETS_PER_QUERY);
        }
    }

    #[test]
    fn parse_vm_vector_extracts_asset_and_value() {
        let json = r#"{
            "status":"success",
            "data":{"resultType":"vector","result":[
                {"metric":{"__name__":"pump_flow_gpm","asset":"PUMP-01"},"value":[1700000000.5,"301.25"]},
                {"metric":{"__name__":"pump_flow_gpm","asset":"PUMP-02"},"value":[1700000000.5,"0"]}
            ]}
        }"#;
        let rows = parse_vm_vector(json).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (Some("PUMP-01".to_string()), 301.25));
        assert_eq!(rows[1], (Some("PUMP-02".to_string()), 0.0));
    }

    #[test]
    fn parse_vm_vector_rejects_error_status() {
        let json = r#"{"status":"error","errorType":"bad_data"}"#;
        assert!(parse_vm_vector(json).is_err());
    }

    #[test]
    fn freshness_respects_ttl_and_quality() {
        let ok = TelemetryValue { value: 1.0, as_of_ms: 1000, ttl_ms: 5000.0, quality: Quality::Ok };
        assert!(ok.is_fresh(5999));
        assert!(!ok.is_fresh(6000)); // as_of + ttl == now -> expired
        let unavail = TelemetryValue { quality: Quality::Unavailable, ..ok };
        assert!(!unavail.is_fresh(1001)); // never fresh -> always retried
    }

    fn req(entity: &str, query: &str) -> ResolveRequest {
        ResolveRequest {
            entity_id: EntityId::from_prim_path(entity),
            attribute: "x".to_string(),
            source_id: "victoriametrics".to_string(),
            query: query.to_string(),
            ttl_ms: 5000.0,
            priority: Priority::Background,
        }
    }
}

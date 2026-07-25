//! Interest / Subscription Manager (spec §3.2) — the control plane that decides
//! *what* must be active given camera AOI, explicit selection, tags, and a
//! budget. It is the analogue of virtual-memory paging / MMO interest
//! management: only entities some subscription references become part of the
//! Runtime Scene Graph.
//!
//! A [`Subscription`] is deliberately **origin-agnostic**: Phase 2 builds them
//! from the CLI, and Phase 5's renderer bridge will build the same struct from
//! the wire, so that wiring is additive rather than a rewrite. This module is
//! pure computation — no I/O, no USD, no telemetry.

use std::collections::{BTreeSet, HashMap};

use crate::lsg::{Aabb, EntityId, Lsg};
use crate::spatial::{self, Frustum, SpatialIndex};

/// Stable identifier for a subscription within one SGS process.
pub type SubscriptionId = u64;

/// Who created the subscription (spec §3.2). Recorded for policy/observability;
/// does not change interest math.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriberKind {
    Viewer,
    Ai,
    AlertRule,
    Automation,
    System,
}

/// Declared viewer class (spec §2.5). Gates the allowed interaction level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerProfile {
    Operator,
    Engineer,
    Observer,
}

/// Interaction level requested by a subscription (spec §2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interaction {
    Full,
    Limited,
    ReadOnly,
}

impl ViewerProfile {
    /// The strongest interaction a profile may request (spec §2.5): `observer`
    /// is read-only, `engineer` limited, `operator` full.
    pub fn max_interaction(self) -> Interaction {
        match self {
            ViewerProfile::Operator => Interaction::Full,
            ViewerProfile::Engineer => Interaction::Limited,
            ViewerProfile::Observer => Interaction::ReadOnly,
        }
    }

    fn rank(i: Interaction) -> u8 {
        match i {
            Interaction::ReadOnly => 0,
            Interaction::Limited => 1,
            Interaction::Full => 2,
        }
    }

    fn allows(self, requested: Interaction) -> bool {
        Self::rank(requested) <= Self::rank(self.max_interaction())
    }
}

/// A spatial trigger region (spec §3.2: `frustum | sphere | aoi_id`).
#[derive(Debug, Clone)]
pub enum Region {
    Sphere { center: [f64; 3], radius: f64 },
    Aabb { min: [f64; 3], max: [f64; 3] },
    Frustum(Frustum),
}

impl Region {
    /// Center used for budget-ordering candidates by proximity.
    fn center(&self) -> [f64; 3] {
        match self {
            Region::Sphere { center, .. } => *center,
            Region::Aabb { min, max } => [
                (min[0] + max[0]) * 0.5,
                (min[1] + max[1]) * 0.5,
                (min[2] + max[2]) * 0.5,
            ],
            Region::Frustum(f) => spatial::centroid(&f.bounds),
        }
    }

    /// Broad-phase candidate ids from the spatial index.
    fn candidates(&self, idx: &SpatialIndex) -> Vec<EntityId> {
        match self {
            Region::Sphere { center, radius } => idx.candidates_for_sphere(*center, *radius),
            Region::Aabb { min, max } => idx.candidates_in_aabb(*min, *max),
            Region::Frustum(f) => idx.candidates_for_frustum(f),
        }
    }

    /// Precise test against an entity's coarse extents.
    fn contains(&self, extents: &Aabb) -> bool {
        match self {
            Region::Sphere { center, radius } => {
                spatial::point_in_sphere(spatial::centroid(extents), *center, *radius)
            }
            Region::Aabb { min, max } => spatial::aabb_overlaps(extents, *min, *max),
            Region::Frustum(f) => f.intersects_aabb(extents),
        }
    }
}

/// A registered interest (spec §3.2). Origin-agnostic: CLI now, bridge later.
#[derive(Debug, Clone)]
pub struct Subscription {
    pub id: SubscriptionId,
    pub kind: SubscriberKind,
    pub viewer_profile: Option<ViewerProfile>,
    /// Spatial AOI trigger. `None` = selection-only.
    pub region: Option<Region>,
    /// Explicit selection: kept in the RSG regardless of the frustum (spec §3.2).
    pub entity_ids: Vec<EntityId>,
    /// Optional tag filter applied to spatial hits (entity must carry one).
    pub tags_filter: Vec<String>,
    /// Max entities this subscription may activate (spec §3.2 budget).
    pub budget: usize,
    pub interaction: Interaction,
}

impl Subscription {
    /// A spatial `operator`-style subscription.
    pub fn spatial(id: SubscriptionId, region: Region, budget: usize) -> Self {
        Subscription {
            id,
            kind: SubscriberKind::Viewer,
            viewer_profile: Some(ViewerProfile::Operator),
            region: Some(region),
            entity_ids: Vec::new(),
            tags_filter: Vec::new(),
            budget,
            interaction: Interaction::Full,
        }
    }

    /// Validate profile/interaction consistency (spec §2.5 / §3.2): a viewer's
    /// profile must permit the requested interaction level.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(profile) = self.viewer_profile {
            if !profile.allows(self.interaction) {
                return Err(format!(
                    "subscription {}: profile {:?} may not request interaction {:?}",
                    self.id, profile, self.interaction
                ));
            }
        }
        Ok(())
    }
}

/// Activate/deactivate transitions produced by one [`InterestManager::evaluate`].
#[derive(Debug, Default, Clone)]
pub struct Transitions {
    pub activated: Vec<(SubscriptionId, EntityId)>,
    pub deactivated: Vec<(SubscriptionId, EntityId)>,
}

impl Transitions {
    pub fn is_empty(&self) -> bool {
        self.activated.is_empty() && self.deactivated.is_empty()
    }
}

/// Holds subscriptions and the last-computed active set per subscription, so
/// each `evaluate` can emit only the deltas.
#[derive(Default)]
pub struct InterestManager {
    subs: HashMap<SubscriptionId, Subscription>,
    active: HashMap<SubscriptionId, BTreeSet<EntityId>>,
}

impl InterestManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a subscription after validating it.
    pub fn upsert(&mut self, sub: Subscription) -> Result<(), String> {
        sub.validate()?;
        self.active.entry(sub.id).or_default();
        self.subs.insert(sub.id, sub);
        Ok(())
    }

    /// Update the spatial region of an existing subscription (camera moved).
    pub fn set_region(&mut self, id: SubscriptionId, region: Region) {
        if let Some(s) = self.subs.get_mut(&id) {
            s.region = Some(region);
        }
    }

    pub fn get(&self, id: SubscriptionId) -> Option<&Subscription> {
        self.subs.get(&id)
    }

    pub fn subscription_count(&self) -> usize {
        self.subs.len()
    }

    /// Remove a subscription; its previously-active entities become deactivate
    /// transitions on the next `evaluate` (they are dropped from `active`).
    pub fn remove(&mut self, id: SubscriptionId) {
        self.subs.remove(&id);
    }

    /// Compute the target set for one subscription: broad-phase spatial
    /// candidates filtered by the precise predicate and tags, unioned with the
    /// explicit selection (which bypasses the frustum), then capped to budget
    /// keeping the entities nearest the region center.
    fn target_set(&self, sub: &Subscription, lsg: &Lsg, idx: &SpatialIndex) -> BTreeSet<EntityId> {
        // Explicit selection first — always retained, bypasses region + tags.
        let mut selection: BTreeSet<EntityId> = BTreeSet::new();
        for id in &sub.entity_ids {
            if lsg.get(*id).is_some() {
                selection.insert(*id);
            }
        }

        // Spatial hits (precise + tag filtered), ordered by proximity to center.
        let mut spatial_hits: Vec<(f64, EntityId)> = Vec::new();
        if let Some(region) = &sub.region {
            let center = region.center();
            for id in region.candidates(idx) {
                if selection.contains(&id) {
                    continue;
                }
                let Some(e) = lsg.get(id) else { continue };
                if !region.contains(&e.extents) {
                    continue;
                }
                if !sub.tags_filter.is_empty()
                    && !sub.tags_filter.iter().any(|t| e.tags.iter().any(|et| et == t))
                {
                    continue;
                }
                let c = spatial::centroid(&e.extents);
                let d = (c[0] - center[0]).powi(2)
                    + (c[1] - center[1]).powi(2)
                    + (c[2] - center[2]).powi(2);
                spatial_hits.push((d, id));
            }
            // Nearest first; tie-break by id for determinism.
            spatial_hits.sort_by(|a, b| {
                a.0.partial_cmp(&b.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.1.cmp(&b.1))
            });
        }

        // Budget: selection is honored first, then as many nearest hits as fit.
        let mut target = selection;
        for (_, id) in spatial_hits {
            if target.len() >= sub.budget {
                break;
            }
            target.insert(id);
        }
        target
    }

    /// Re-evaluate every subscription against the current LSG + spatial index,
    /// returning the activate/deactivate deltas since the previous call.
    pub fn evaluate(&mut self, lsg: &Lsg, idx: &SpatialIndex) -> Transitions {
        let mut transitions = Transitions::default();

        // Deactivate everything for subscriptions that were removed.
        let live: BTreeSet<SubscriptionId> = self.subs.keys().copied().collect();
        let stale: Vec<SubscriptionId> = self
            .active
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in stale {
            if let Some(prev) = self.active.remove(&id) {
                for e in prev {
                    transitions.deactivated.push((id, e));
                }
            }
        }

        // Collect subscription ids up front (sorted) for deterministic output.
        let mut ids: Vec<SubscriptionId> = self.subs.keys().copied().collect();
        ids.sort();
        for id in ids {
            let sub = self.subs.get(&id).expect("id came from subs").clone();
            let target = self.target_set(&sub, lsg, idx);
            let prev = self.active.entry(id).or_default();

            for e in target.difference(prev) {
                transitions.activated.push((id, *e));
            }
            for e in prev.difference(&target) {
                transitions.deactivated.push((id, *e));
            }
            *prev = target;
        }
        transitions
    }

    /// Current active entity count for a subscription (diagnostics).
    pub fn active_count(&self, id: SubscriptionId) -> usize {
        self.active.get(&id).map(|s| s.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsg::{Entity, Transform};

    fn ent(path: &str, tag: &str, center: [f64; 3], tags: &[&str]) -> Entity {
        Entity {
            id: EntityId::from_prim_path(path),
            prim_path: path.to_string(),
            parent: None,
            children: vec![],
            kind: Some("component".to_string()),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            vf: {
                let mut m = std::collections::HashMap::new();
                m.insert("assetTag".to_string(), serde_json::Value::String(tag.to_string()));
                m
            },
            transform_default: Transform::from_translation(center),
            extents: Aabb {
                min: [center[0] - 0.5, center[1] - 0.5, center[2] - 0.5],
                max: [center[0] + 0.5, center[1] + 0.5, center[2] + 0.5],
            },
            geom_ref: None,
            bindings: vec![],
        }
    }

    fn scene() -> (Lsg, SpatialIndex) {
        let mut lsg = Lsg::new();
        lsg.insert(ent("/e0", "A", [0.0, 0.0, 0.0], &["pump"]));
        lsg.insert(ent("/e1", "B", [4.0, 0.0, 0.0], &["pump"]));
        lsg.insert(ent("/e2", "C", [100.0, 0.0, 0.0], &["tank"]));
        lsg.link_hierarchy();
        let idx = SpatialIndex::build(&lsg);
        (lsg, idx)
    }

    #[test]
    fn aoi_activates_only_nearby() {
        let (lsg, idx) = scene();
        let mut im = InterestManager::new();
        im.upsert(Subscription::spatial(
            1,
            Region::Sphere { center: [0.0, 0.0, 0.0], radius: 6.0 },
            100,
        ))
        .unwrap();
        let t = im.evaluate(&lsg, &idx);
        let active: BTreeSet<EntityId> = t.activated.iter().map(|(_, e)| *e).collect();
        assert!(active.contains(&EntityId::from_prim_path("/e0")));
        assert!(active.contains(&EntityId::from_prim_path("/e1")));
        assert!(!active.contains(&EntityId::from_prim_path("/e2")));
    }

    #[test]
    fn selection_retained_outside_region() {
        let (lsg, idx) = scene();
        let mut im = InterestManager::new();
        let mut sub = Subscription::spatial(
            1,
            Region::Sphere { center: [0.0, 0.0, 0.0], radius: 1.0 },
            100,
        );
        // Select the far entity explicitly; it must be active despite the AOI.
        sub.entity_ids = vec![EntityId::from_prim_path("/e2")];
        im.upsert(sub).unwrap();
        let t = im.evaluate(&lsg, &idx);
        let active: BTreeSet<EntityId> = t.activated.iter().map(|(_, e)| *e).collect();
        assert!(active.contains(&EntityId::from_prim_path("/e2")));
    }

    #[test]
    fn budget_caps_to_nearest() {
        let (lsg, idx) = scene();
        let mut im = InterestManager::new();
        im.upsert(Subscription::spatial(
            1,
            Region::Sphere { center: [0.0, 0.0, 0.0], radius: 10.0 },
            1, // only one slot
        ))
        .unwrap();
        let t = im.evaluate(&lsg, &idx);
        assert_eq!(t.activated.len(), 1);
        // Nearest to center (0,0,0) is /e0.
        assert_eq!(t.activated[0].1, EntityId::from_prim_path("/e0"));
    }

    #[test]
    fn tags_filter_narrows_hits() {
        let (lsg, idx) = scene();
        let mut im = InterestManager::new();
        let mut sub = Subscription::spatial(
            1,
            Region::Sphere { center: [0.0, 0.0, 0.0], radius: 10.0 },
            100,
        );
        sub.tags_filter = vec!["tank".to_string()];
        im.upsert(sub).unwrap();
        let t = im.evaluate(&lsg, &idx);
        // Only tank-tagged entities within range; e0/e1 are pumps, e2 is far.
        assert!(t.activated.is_empty());
    }

    #[test]
    fn observer_cannot_request_full_interaction() {
        let mut im = InterestManager::new();
        let mut sub = Subscription::spatial(
            1,
            Region::Sphere { center: [0.0; 3], radius: 1.0 },
            10,
        );
        sub.viewer_profile = Some(ViewerProfile::Observer);
        sub.interaction = Interaction::Full;
        assert!(im.upsert(sub).is_err());
    }

    #[test]
    fn moving_region_emits_deltas() {
        let (lsg, idx) = scene();
        let mut im = InterestManager::new();
        im.upsert(Subscription::spatial(
            1,
            Region::Sphere { center: [0.0, 0.0, 0.0], radius: 2.0 },
            100,
        ))
        .unwrap();
        let _ = im.evaluate(&lsg, &idx); // e0 active
        // Move AOI to e1.
        im.set_region(1, Region::Sphere { center: [4.0, 0.0, 0.0], radius: 2.0 });
        let t = im.evaluate(&lsg, &idx);
        let act: BTreeSet<EntityId> = t.activated.iter().map(|(_, e)| *e).collect();
        let deact: BTreeSet<EntityId> = t.deactivated.iter().map(|(_, e)| *e).collect();
        assert!(act.contains(&EntityId::from_prim_path("/e1")));
        assert!(deact.contains(&EntityId::from_prim_path("/e0")));
    }
}

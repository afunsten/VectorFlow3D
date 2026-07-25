//! Runtime Scene Graph (spec §3.3/§4.4): the materialized working set produced
//! by applying interest [`Transitions`] to the LSG index.
//!
//! Design invariants enforced here:
//! - **Shared pages, not per-viewer copies** (spec §3.3): one [`RuntimeEntity`]
//!   per active entity, refcounted by the set of subscriptions referencing it.
//!   Multiple subscribers on the same entity share one page and one hydrated
//!   payload.
//! - **Eviction on last release + grace** (spec §3.3): when no subscription
//!   references an entity, it is scheduled for eviction; only after a grace
//!   period does its page drop and its payload unload. The SAME rule governs
//!   payload unload — there is no separate payload eviction policy.
//! - **Per-subscriber diff cursors**: each subscription accumulates the
//!   upsert/remove deltas for *its* view — the seam Phase 5's renderer bridge
//!   drains into `vf.bridge.v1`.
//! - **No writes to USD / LSG / Twin Overlay**: hydration is read-only and this
//!   module only reads the LSG.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::Result;

use crate::hydrate::{HydratedPayload, PayloadCache};
use crate::interest::{SubscriptionId, Transitions};
use crate::lsg::{EntityId, Lsg};
use crate::resolver::TelemetryValue;

/// A materialized entity in the working set. USD-seeded fields stay in the LSG;
/// this holds only runtime facts (spec §4.4). Telemetry is Phase 3.
#[derive(Debug, Clone)]
pub struct RuntimeEntity {
    pub id: EntityId,
    pub prim_path: String,
    pub lsg_revision: u64,
    /// Subscriptions currently referencing this entity (the refcount).
    pub subscribers: BTreeSet<SubscriptionId>,
    /// Component-internal defaults surfaced by opening the payload (Phase 2).
    pub hydrated: Option<Arc<HydratedPayload>>,
    /// Cache key for the hydrated payload, so eviction can release it.
    payload_key: Option<String>,
    /// Lazily-resolved telemetry cache keyed by attribute (spec §4.4, Phase 3).
    /// Populated by [`crate::resolver::resolve_active`]; never persisted to USD.
    pub telemetry: HashMap<String, TelemetryValue>,
}

/// Per-subscriber view delta — the seam Phase 5 consumes.
#[derive(Debug, Default, Clone)]
pub struct RsgDiff {
    pub upserts: Vec<EntityId>,
    pub removes: Vec<EntityId>,
}

impl RsgDiff {
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.removes.is_empty()
    }
}

/// The Runtime Scene Graph working set.
pub struct Rsg {
    entities: HashMap<EntityId, RuntimeEntity>,
    /// Entities with zero subscribers awaiting eviction: id -> tick when the
    /// grace period expires.
    pending_evict: HashMap<EntityId, u64>,
    /// Grace period in ticks before an unreferenced entity is dropped.
    grace: u64,
    /// Accumulated per-subscriber diffs, drained by the caller.
    diffs: HashMap<SubscriptionId, RsgDiff>,
}

impl Rsg {
    pub fn new(grace: u64) -> Self {
        Rsg {
            entities: HashMap::new(),
            pending_evict: HashMap::new(),
            grace,
            diffs: HashMap::new(),
        }
    }

    /// Apply interest transitions at logical time `now` (a monotonic tick).
    /// Activations insert/refcount pages and hydrate payloads; deactivations
    /// drop the subscriber and, when the page hits zero refs, schedule eviction.
    pub fn apply(
        &mut self,
        transitions: &Transitions,
        lsg: &Lsg,
        cache: &mut PayloadCache,
        now: u64,
    ) -> Result<()> {
        for (sub, id) in &transitions.activated {
            self.activate(*sub, *id, lsg, cache)?;
        }
        for (sub, id) in &transitions.deactivated {
            self.deactivate(*sub, *id, now);
        }
        Ok(())
    }

    fn activate(
        &mut self,
        sub: SubscriptionId,
        id: EntityId,
        lsg: &Lsg,
        cache: &mut PayloadCache,
    ) -> Result<()> {
        // Re-referenced before its grace expired: cancel pending eviction.
        self.pending_evict.remove(&id);

        if let Some(re) = self.entities.get_mut(&id) {
            re.subscribers.insert(sub);
        } else {
            let Some(e) = lsg.get(id) else {
                return Ok(()); // vanished from the index; nothing to materialize
            };
            let (payload_key, hydrated) = match &e.geom_ref {
                Some(g) => {
                    let (key, payload) = cache.acquire(g)?;
                    (Some(key), Some(payload))
                }
                None => (None, None),
            };
            let mut subscribers = BTreeSet::new();
            subscribers.insert(sub);
            self.entities.insert(
                id,
                RuntimeEntity {
                    id,
                    prim_path: e.prim_path.clone(),
                    lsg_revision: lsg.revision(),
                    subscribers,
                    hydrated,
                    payload_key,
                    telemetry: HashMap::new(),
                },
            );
        }
        self.diffs.entry(sub).or_default().upserts.push(id);
        Ok(())
    }

    fn deactivate(&mut self, sub: SubscriptionId, id: EntityId, now: u64) {
        if let Some(re) = self.entities.get_mut(&id) {
            re.subscribers.remove(&sub);
            if re.subscribers.is_empty() {
                // Last subscriber gone: start the grace timer (shared with the
                // payload — no separate payload eviction policy).
                self.pending_evict.insert(id, now.saturating_add(self.grace));
            }
        }
        self.diffs.entry(sub).or_default().removes.push(id);
    }

    /// Evict entities whose grace period has expired and that still have no
    /// subscribers; unloads their payloads from the cache. Returns evicted ids.
    pub fn evict_expired(&mut self, now: u64, cache: &mut PayloadCache) -> Vec<EntityId> {
        let due: Vec<EntityId> = self
            .pending_evict
            .iter()
            .filter(|(_, &deadline)| now >= deadline)
            .map(|(id, _)| *id)
            .collect();

        let mut evicted = Vec::new();
        for id in due {
            self.pending_evict.remove(&id);
            // Only evict if it is still unreferenced (may have been re-acquired).
            match self.entities.get(&id) {
                Some(re) if re.subscribers.is_empty() => {}
                _ => continue,
            }
            if let Some(re) = self.entities.remove(&id) {
                if let Some(key) = re.payload_key {
                    cache.release(&key);
                }
                evicted.push(id);
            }
        }
        evicted
    }

    /// Drain the accumulated diff for a subscription (empties its cursor).
    pub fn take_diff(&mut self, sub: SubscriptionId) -> RsgDiff {
        self.diffs.remove(&sub).unwrap_or_default()
    }

    /// Clear all accumulated diffs (e.g. after a demo step is reported).
    pub fn clear_diffs(&mut self) {
        self.diffs.clear();
    }

    pub fn get(&self, id: EntityId) -> Option<&RuntimeEntity> {
        self.entities.get(&id)
    }

    /// Mutable access to a materialized entity (used by the telemetry resolve
    /// pass to write into its RSG-resident cache, spec §4.4).
    pub fn get_mut(&mut self, id: EntityId) -> Option<&mut RuntimeEntity> {
        self.entities.get_mut(&id)
    }

    /// Iterate over materialized entities (diagnostics / demo output).
    pub fn entities(&self) -> impl Iterator<Item = &RuntimeEntity> {
        self.entities.values()
    }

    pub fn contains(&self, id: EntityId) -> bool {
        self.entities.contains_key(&id)
    }

    /// Number of materialized entities (`|RSG|`, spec §6.3).
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Entities awaiting eviction (unreferenced, within grace).
    pub fn pending_eviction_count(&self) -> usize {
        self.pending_evict.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hydrate::StubPayloadLoader;
    use crate::lsg::{Aabb, Entity, GeomRef, Transform};

    fn ent(path: &str, with_payload: bool) -> Entity {
        Entity {
            id: EntityId::from_prim_path(path),
            prim_path: path.to_string(),
            parent: None,
            children: vec![],
            kind: Some("component".to_string()),
            tags: vec![],
            vf: Default::default(),
            transform_default: Transform::identity(),
            extents: Aabb::zero(),
            geom_ref: if with_payload {
                Some(GeomRef {
                    payload_uri: "./components/pump.usda".to_string(),
                    prim_path: "/Pump".to_string(),
                    content_hash: "hash-pump".to_string(),
                    lod_ladder: vec![],
                })
            } else {
                None
            },
            bindings: vec![],
        }
    }

    fn lsg2() -> Lsg {
        let mut lsg = Lsg::new();
        lsg.insert(ent("/a", true));
        lsg.insert(ent("/b", true));
        lsg.link_hierarchy();
        lsg
    }

    fn trans(activated: &[(SubscriptionId, &str)], deactivated: &[(SubscriptionId, &str)]) -> Transitions {
        Transitions {
            activated: activated
                .iter()
                .map(|(s, p)| (*s, EntityId::from_prim_path(p)))
                .collect(),
            deactivated: deactivated
                .iter()
                .map(|(s, p)| (*s, EntityId::from_prim_path(p)))
                .collect(),
        }
    }

    #[test]
    fn shared_page_refcounts_across_subscribers() {
        let lsg = lsg2();
        let mut cache = PayloadCache::new(Box::new(StubPayloadLoader));
        let mut rsg = Rsg::new(2);
        let a = EntityId::from_prim_path("/a");

        // Two subscribers activate the same entity.
        rsg.apply(&trans(&[(1, "/a")], &[]), &lsg, &mut cache, 0).unwrap();
        rsg.apply(&trans(&[(2, "/a")], &[]), &lsg, &mut cache, 0).unwrap();
        assert_eq!(rsg.len(), 1);
        assert_eq!(rsg.get(a).unwrap().subscribers.len(), 2);
        assert_eq!(cache.loaded_count(), 1);
        assert_eq!(cache.load_calls(), 1); // hydrated once, shared

        // Sub 1 leaves — page stays (sub 2 still references it), no eviction.
        rsg.apply(&trans(&[], &[(1, "/a")]), &lsg, &mut cache, 1).unwrap();
        assert_eq!(rsg.pending_eviction_count(), 0);
        assert_eq!(rsg.len(), 1);
    }

    #[test]
    fn eviction_waits_for_grace_then_unloads_payload() {
        let lsg = lsg2();
        let mut cache = PayloadCache::new(Box::new(StubPayloadLoader));
        let mut rsg = Rsg::new(3);
        let a = EntityId::from_prim_path("/a");

        rsg.apply(&trans(&[(1, "/a")], &[]), &lsg, &mut cache, 0).unwrap();
        rsg.apply(&trans(&[], &[(1, "/a")]), &lsg, &mut cache, 0).unwrap();
        assert_eq!(rsg.pending_eviction_count(), 1);

        // Before grace expires: nothing evicted.
        assert!(rsg.evict_expired(2, &mut cache).is_empty());
        assert_eq!(rsg.len(), 1);
        assert_eq!(cache.loaded_count(), 1);

        // At/after grace: evicted and payload unloaded.
        let evicted = rsg.evict_expired(3, &mut cache);
        assert_eq!(evicted, vec![a]);
        assert_eq!(rsg.len(), 0);
        assert_eq!(cache.loaded_count(), 0);
    }

    #[test]
    fn reacquire_before_grace_cancels_eviction() {
        let lsg = lsg2();
        let mut cache = PayloadCache::new(Box::new(StubPayloadLoader));
        let mut rsg = Rsg::new(5);
        let a = EntityId::from_prim_path("/a");

        rsg.apply(&trans(&[(1, "/a")], &[]), &lsg, &mut cache, 0).unwrap();
        rsg.apply(&trans(&[], &[(1, "/a")]), &lsg, &mut cache, 1).unwrap();
        assert_eq!(rsg.pending_eviction_count(), 1);

        // Re-activated (by any sub) before grace: eviction cancelled.
        rsg.apply(&trans(&[(2, "/a")], &[]), &lsg, &mut cache, 2).unwrap();
        assert_eq!(rsg.pending_eviction_count(), 0);
        assert!(rsg.evict_expired(100, &mut cache).is_empty());
        assert_eq!(rsg.len(), 1);
        assert!(rsg.get(a).unwrap().subscribers.contains(&2));
    }

    #[test]
    fn per_subscriber_diffs_track_own_view() {
        let lsg = lsg2();
        let mut cache = PayloadCache::new(Box::new(StubPayloadLoader));
        let mut rsg = Rsg::new(2);

        rsg.apply(&trans(&[(1, "/a"), (2, "/b")], &[]), &lsg, &mut cache, 0).unwrap();
        let d1 = rsg.take_diff(1);
        let d2 = rsg.take_diff(2);
        assert_eq!(d1.upserts, vec![EntityId::from_prim_path("/a")]);
        assert_eq!(d2.upserts, vec![EntityId::from_prim_path("/b")]);
        // Draining empties the cursor.
        assert!(rsg.take_diff(1).is_empty());
    }
}

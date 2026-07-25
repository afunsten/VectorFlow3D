//! Twin-Overlay *opinions* — the durable, renderer-independent edits the Flow3D
//! DSL lowers into (spec §3.9 "DSL compiles into LSG opinions … with stable
//! IDs"; §4.5 binding overrides / compile stamps in the Twin Overlay).
//!
//! An [`Opinion`] is the unit of both persistence (one SQLite row) and
//! incremental reload:
//! - [`Opinion::key`] identifies the *slot* an opinion occupies, so editing it
//!   across recompiles reads as a **change** rather than a remove + add.
//! - [`Opinion::content_hash`] captures its payload, so an unchanged slot is
//!   detected as **unchanged** and never re-applied — this is what keeps a
//!   reload from storming the RSG (spec Phase 4 "reload patches without RSG
//!   storm").
//!
//! Opinions are additive overlay edits: applying them mutates the in-memory LSG
//! index in place and is persisted to SQLite, but **never** writes USD (the
//! "vendor geom layers remain untouched" invariant).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::lsg::{Anchor, Edge, EntityId, Lsg, TelemetryBinding};

/// A single durable twin edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Opinion {
    /// Add/override a declarative telemetry binding on an entity.
    Binding {
        entity: EntityId,
        binding: TelemetryBinding,
    },
    /// Add a semantic tag to an entity.
    Tag { entity: EntityId, tag: String },
    /// Set a `customData.vf` metadata key on an entity.
    Meta {
        entity: EntityId,
        key: String,
        value: String,
    },
    /// A DSL-authored anchor point.
    Anchor(Anchor),
    /// A DSL-authored edge/pipe.
    Edge(Edge),
}

impl Opinion {
    /// Stable slot identity used for the reload diff. Same key across recompiles
    /// = the same logical opinion (its content may change).
    pub fn key(&self) -> String {
        match self {
            Opinion::Binding { entity, binding } => {
                format!("bind:{}:{}", entity.as_hex(), binding.attribute)
            }
            Opinion::Tag { entity, tag } => format!("tag:{}:{}", entity.as_hex(), tag),
            Opinion::Meta { entity, key, .. } => format!("meta:{}:{}", entity.as_hex(), key),
            Opinion::Anchor(a) => format!("anchor:{}", a.id.as_hex()),
            Opinion::Edge(e) => format!("edge:{}", e.id.as_hex()),
        }
    }

    /// Content hash over the full payload (sha256 hex). Two opinions with the
    /// same [`key`](Opinion::key) but different hashes represent an edit.
    pub fn content_hash(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_default();
        let mut h = Sha256::new();
        h.update(json.as_bytes());
        format!("{:x}", h.finalize())
    }

    /// Kind tag stored alongside the row (diagnostics / filtering).
    pub fn kind(&self) -> &'static str {
        match self {
            Opinion::Binding { .. } => "binding",
            Opinion::Tag { .. } => "tag",
            Opinion::Meta { .. } => "meta",
            Opinion::Anchor(_) => "anchor",
            Opinion::Edge(_) => "edge",
        }
    }

    /// The primary entity an opinion touches (for the "touched entities" count
    /// that proves a minimal patch). Edges attribute to their `from` entity.
    pub fn primary_entity(&self) -> EntityId {
        match self {
            Opinion::Binding { entity, .. }
            | Opinion::Tag { entity, .. }
            | Opinion::Meta { entity, .. } => *entity,
            Opinion::Anchor(a) => a.entity,
            Opinion::Edge(e) => e.from.0,
        }
    }
}

/// The result of diffing a freshly-compiled opinion set against the previously
/// persisted one (keyed by [`Opinion::key`] → content hash).
#[derive(Debug, Default, Clone)]
pub struct OpinionDiff {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: Vec<String>,
}

impl OpinionDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    /// Number of opinion slots that actually moved (added + changed + removed).
    pub fn touched(&self) -> usize {
        self.added.len() + self.changed.len() + self.removed.len()
    }
}

/// Diff the new opinion set against previously-persisted `prev` (key → hash).
pub fn diff(new: &[Opinion], prev: &HashMap<String, String>) -> OpinionDiff {
    let mut out = OpinionDiff::default();
    let mut new_keys = std::collections::HashSet::new();
    for op in new {
        let key = op.key();
        let hash = op.content_hash();
        match prev.get(&key) {
            None => out.added.push(key.clone()),
            Some(prev_hash) if *prev_hash != hash => out.changed.push(key.clone()),
            Some(_) => out.unchanged.push(key.clone()),
        }
        new_keys.insert(key);
    }
    for key in prev.keys() {
        if !new_keys.contains(key) {
            out.removed.push(key.clone());
        }
    }
    out
}

/// Distinct entities referenced by a set of opinions (touched-entity count).
pub fn touched_entities(opinions: &[Opinion]) -> std::collections::BTreeSet<EntityId> {
    opinions.iter().map(|o| o.primary_entity()).collect()
}

/// Apply opinions onto the LSG index in place, bumping the revision once.
/// Returns the set of entities whose records changed.
pub fn apply(lsg: &mut Lsg, opinions: &[Opinion]) -> std::collections::BTreeSet<EntityId> {
    let mut touched = std::collections::BTreeSet::new();
    for op in opinions {
        let changed = match op {
            Opinion::Binding { entity, binding } => lsg.upsert_binding(*entity, binding.clone()),
            Opinion::Tag { entity, tag } => lsg.add_tag(*entity, tag),
            Opinion::Meta { entity, key, value } => {
                lsg.set_vf(*entity, key, serde_json::Value::String(value.clone()))
            }
            Opinion::Anchor(a) => {
                lsg.insert_anchor(a.clone());
                true
            }
            Opinion::Edge(e) => {
                lsg.insert_edge(e.clone());
                true
            }
        };
        if changed {
            touched.insert(op.primary_entity());
        }
    }
    if !touched.is_empty() || !opinions.is_empty() {
        lsg.bump_revision();
    }
    touched
}

/// Reconcile the LSG from `prev` to `new` opinions as a **minimal in-place
/// patch**: apply only added/changed opinions and unapply only removed ones,
/// leaving unchanged slots (and their `EntityId`s) untouched. This is what
/// makes a DSL reload patch rather than rebuild (spec Phase 4). Returns the
/// distinct entities actually mutated.
pub fn reconcile(
    lsg: &mut Lsg,
    prev: &[Opinion],
    new: &[Opinion],
) -> std::collections::BTreeSet<EntityId> {
    let prev_map: HashMap<String, &Opinion> = prev.iter().map(|o| (o.key(), o)).collect();
    let new_map: HashMap<String, &Opinion> = new.iter().map(|o| (o.key(), o)).collect();

    let mut to_apply = Vec::new();
    for (key, op) in &new_map {
        match prev_map.get(key) {
            None => to_apply.push((*op).clone()),
            Some(prev_op) if prev_op.content_hash() != op.content_hash() => {
                to_apply.push((*op).clone())
            }
            Some(_) => {}
        }
    }
    let mut to_remove = Vec::new();
    for (key, op) in &prev_map {
        if !new_map.contains_key(key) {
            to_remove.push((*op).clone());
        }
    }

    let mut touched = apply(lsg, &to_apply);
    touched.extend(unapply(lsg, &to_remove));
    touched
}

/// Reverse [`apply`] for a set of opinions (used when a reload *removes* them).
pub fn unapply(lsg: &mut Lsg, opinions: &[Opinion]) -> std::collections::BTreeSet<EntityId> {
    let mut touched = std::collections::BTreeSet::new();
    for op in opinions {
        let changed = match op {
            Opinion::Binding { entity, binding } => lsg.remove_binding(*entity, &binding.attribute),
            Opinion::Tag { entity, tag } => lsg.remove_tag(*entity, tag),
            Opinion::Meta { entity, key, .. } => lsg.remove_vf(*entity, key),
            Opinion::Anchor(a) => lsg.remove_anchor(a.id),
            Opinion::Edge(e) => lsg.remove_edge(e.id),
        };
        if changed {
            touched.insert(op.primary_entity());
        }
    }
    if !touched.is_empty() {
        lsg.bump_revision();
    }
    touched
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding_op(entity: EntityId, attr: &str, query: &str) -> Opinion {
        Opinion::Binding {
            entity,
            binding: TelemetryBinding {
                attribute: attr.to_string(),
                source_id: "victoriametrics".into(),
                query: query.to_string(),
                unit: String::new(),
                ttl_ms: 5000.0,
                priority: "background".into(),
                quality_policy: "stale_ok".into(),
            },
        }
    }

    #[test]
    fn diff_classifies_added_changed_removed_unchanged() {
        let e = EntityId::from_prim_path("/A");
        let v1 = vec![
            binding_op(e, "flow", "q1"),
            Opinion::Tag { entity: e, tag: "duty".into() },
        ];
        let mut prev = HashMap::new();
        for op in &v1 {
            prev.insert(op.key(), op.content_hash());
        }

        let v2 = vec![
            binding_op(e, "flow", "q2"), // changed query
            binding_op(e, "temp", "q3"), // added
                                         // "duty" tag removed
        ];
        let d = diff(&v2, &prev);
        assert_eq!(d.changed.len(), 1, "{d:?}");
        assert_eq!(d.added.len(), 1, "{d:?}");
        assert_eq!(d.removed.len(), 1, "{d:?}");
        assert_eq!(d.unchanged.len(), 0, "{d:?}");
    }

    #[test]
    fn unchanged_reload_touches_nothing() {
        let e = EntityId::from_prim_path("/A");
        let v = vec![binding_op(e, "flow", "q1")];
        let mut prev = HashMap::new();
        for op in &v {
            prev.insert(op.key(), op.content_hash());
        }
        let d = diff(&v, &prev);
        assert!(d.is_empty());
        assert_eq!(d.touched(), 0);
    }
}

//! Logical Scene Graph (LSG): the authoritative, always-resident index that is
//! initialized from an OpenUSD stage (payloads unloaded) plus the Twin Overlay.
//!
//! Spec §3.1 / §0 glossary: the LSG describes what exists, where (coarse
//! extents), how to hydrate it later (`GeomRef`), and how to resolve telemetry
//! (binding descriptors) — WITHOUT holding live values, GPU resources, or a
//! duplicate geometry database. Everything the runtime scales/overrides from
//! telemetry treats the USD-derived values here as *defaults*.

use std::collections::HashMap;
use std::hash::Hasher;

use serde::{Deserialize, Serialize};

/// Stable, non-random identity for an entity — a hash of its prim path
/// (spec §3.1/§4.2: `EntityId = stable hash(prim_path | asset_id)`).
///
/// Determinism matters: pins in the Twin Overlay key off this value across
/// process restarts, so we use a fixed-seed xxhash rather than the process's
/// randomized `DefaultHasher`.
///
/// **Wire encoding:** a full 64-bit id exceeds JavaScript's safe-integer range
/// (2^53), so a bare JSON number would be silently corrupted by the browser
/// observer client's `JSON.parse`, breaking pick/pin id round-trips. `EntityId`
/// therefore (de)serializes as a **16-char hex string** (matching [`as_hex`] /
/// [`Display`] and the Twin Overlay's `entity_id TEXT` column) so ids survive
/// the WebSocket path losslessly across languages. Still `vf.bridge.v1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityId(pub u64);

impl EntityId {
    pub fn from_prim_path(prim_path: &str) -> Self {
        let mut h = twox_hash::XxHash64::with_seed(0);
        h.write(prim_path.as_bytes());
        EntityId(h.finish())
    }

    pub fn as_hex(&self) -> String {
        format!("{:016x}", self.0)
    }
}

impl Serialize for EntityId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_hex())
    }
}

impl<'de> Deserialize<'de> for EntityId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        u64::from_str_radix(&s, 16)
            .map(EntityId)
            .map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_hex())
    }
}

/// A 4x4 transform stored row-major. Authored transforms from USD are
/// *defaults*; the Twin Overlay may pin a stronger opinion (spec §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    /// Row-major 4x4 matrix.
    pub matrix: [f64; 16],
}

impl Transform {
    pub fn identity() -> Self {
        let mut m = [0.0; 16];
        m[0] = 1.0;
        m[5] = 1.0;
        m[10] = 1.0;
        m[15] = 1.0;
        Transform { matrix: m }
    }

    pub fn from_translation(t: [f64; 3]) -> Self {
        let mut m = Self::identity();
        // Row-major: translation in the last column of the first three rows.
        m.matrix[3] = t[0];
        m.matrix[7] = t[1];
        m.matrix[11] = t[2];
        m
    }

    /// Extract the translation component (last column, rows 0..3).
    pub fn translation(&self) -> [f64; 3] {
        [self.matrix[3], self.matrix[7], self.matrix[11]]
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self::identity()
    }
}

/// Coarse axis-aligned bounding box used for interest queries (spec §3.1).
/// Seeded from authored `extentsHint` / USD bounds at import.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Aabb {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Aabb {
    pub fn zero() -> Self {
        Aabb {
            min: [0.0; 3],
            max: [0.0; 3],
        }
    }
}

/// Handle into the (future) VF geometry store, seeded from a USD payload/layer
/// at import (spec §4.2). Phase 1 records the reference + a content hash; it
/// never opens the payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeomRef {
    /// Payload asset URI as authored (e.g. `./components/pump.usda`).
    pub payload_uri: String,
    /// Target prim inside the payload (e.g. `/Pump`).
    pub prim_path: String,
    /// Content hash of the referenced payload file (sha256 hex), or empty if
    /// the file could not be read at import.
    #[serde(default)]
    pub content_hash: String,
    /// LOD ladder handles (empty in Phase 1).
    #[serde(default)]
    pub lod_ladder: Vec<String>,
}

/// Declarative telemetry binding descriptor (spec §3.4/§4.2). Describes *how to
/// resolve* a value; never the value itself. Not resolved in Phase 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryBinding {
    pub attribute: String,
    pub source_id: String,
    pub query: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub ttl_ms: f64,
    #[serde(default)]
    pub priority: String,
    #[serde(default)]
    pub quality_policy: String,
}

/// Stable id for a DSL-authored anchor point on an entity. Derived from the
/// owning entity + anchor name so it survives DSL recompiles (spec §3.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AnchorId(pub u64);

impl AnchorId {
    pub fn new(entity: EntityId, name: &str) -> Self {
        let mut h = twox_hash::XxHash64::with_seed(0);
        h.write(entity.as_hex().as_bytes());
        h.write(b"#anchor#");
        h.write(name.as_bytes());
        AnchorId(h.finish())
    }

    pub fn as_hex(&self) -> String {
        format!("{:016x}", self.0)
    }
}

/// Stable id for a DSL-authored edge/pipe between two anchors. Derived from the
/// ordered endpoints so it survives recompiles (spec §4.2 `Edge / Pipe`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EdgeId(pub u64);

impl EdgeId {
    pub fn new(from: (EntityId, &str), to: (EntityId, &str)) -> Self {
        let mut h = twox_hash::XxHash64::with_seed(0);
        h.write(from.0.as_hex().as_bytes());
        h.write(b".");
        h.write(from.1.as_bytes());
        h.write(b"->");
        h.write(to.0.as_hex().as_bytes());
        h.write(b".");
        h.write(to.1.as_bytes());
        EdgeId(h.finish())
    }

    pub fn as_hex(&self) -> String {
        format!("{:016x}", self.0)
    }
}

/// An anchor point on an entity (spec §3.9: "pipes/anchors"). Authored by the
/// Flow3D DSL and held as an LSG opinion — never a live value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub id: AnchorId,
    pub entity: EntityId,
    pub name: String,
    /// Local offset from the entity origin.
    pub pos: [f64; 3],
}

/// Connectivity between two anchors (spec §4.2 `Edge / Pipe`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub from: (EntityId, String),
    pub to: (EntityId, String),
}

/// One indexed prim. All USD-derived fields are *defaults* (spec §3.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: EntityId,
    pub prim_path: String,
    pub parent: Option<EntityId>,
    #[serde(default)]
    pub children: Vec<EntityId>,
    /// USD `kind` (assembly | group | component | ...), if authored.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Composed `customData.vf` dictionary (identity/static metadata defaults).
    #[serde(default)]
    pub vf: HashMap<String, serde_json::Value>,
    pub transform_default: Transform,
    pub extents: Aabb,
    #[serde(default)]
    pub geom_ref: Option<GeomRef>,
    #[serde(default)]
    pub bindings: Vec<TelemetryBinding>,
}

impl Entity {
    /// The `vf.assetTag` string, if present (the PromQL `asset` label carrier).
    pub fn asset_tag(&self) -> Option<&str> {
        self.vf.get("assetTag").and_then(|v| v.as_str())
    }

    /// The `vf.class` string, if present.
    pub fn class(&self) -> Option<&str> {
        self.vf.get("class").and_then(|v| v.as_str())
    }
}

/// The Logical Scene Graph index over an imported stage.
#[derive(Debug, Default)]
pub struct Lsg {
    entities: HashMap<EntityId, Entity>,
    by_path: HashMap<String, EntityId>,
    by_asset_tag: HashMap<String, EntityId>,
    /// attribute name -> entities that bind it (binding index, spec §3.1).
    binding_index: HashMap<String, Vec<EntityId>>,
    /// DSL-authored anchors (spec §3.9), keyed by stable id.
    anchors: HashMap<AnchorId, Anchor>,
    /// DSL-authored edges/pipes (spec §4.2 `Scene = { …, edges, … }`).
    edges: HashMap<EdgeId, Edge>,
    /// Monotonic index revision (bumped on structural mutation).
    revision: u64,
}

impl Lsg {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert an entity, updating the path / asset-tag / binding indexes. Does
    /// not link parent<->child; call [`Lsg::link_hierarchy`] once after bulk
    /// insert.
    pub fn insert(&mut self, entity: Entity) {
        self.by_path.insert(entity.prim_path.clone(), entity.id);
        if let Some(tag) = entity.asset_tag() {
            self.by_asset_tag.insert(tag.to_string(), entity.id);
        }
        for b in &entity.bindings {
            self.binding_index
                .entry(b.attribute.clone())
                .or_default()
                .push(entity.id);
        }
        self.entities.insert(entity.id, entity);
    }

    /// Populate each entity's `children` from the `parent` links. Idempotent.
    pub fn link_hierarchy(&mut self) {
        for e in self.entities.values_mut() {
            e.children.clear();
        }
        let pairs: Vec<(EntityId, EntityId)> = self
            .entities
            .values()
            .filter_map(|e| e.parent.map(|p| (p, e.id)))
            .collect();
        for (parent, child) in pairs {
            if let Some(pe) = self.entities.get_mut(&parent) {
                pe.children.push(child);
            }
        }
        for e in self.entities.values_mut() {
            e.children.sort();
        }
        self.revision += 1;
    }

    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        self.entities.get(&id)
    }

    pub fn by_path(&self, prim_path: &str) -> Option<&Entity> {
        self.by_path.get(prim_path).and_then(|id| self.entities.get(id))
    }

    pub fn by_asset_tag(&self, tag: &str) -> Option<&Entity> {
        self.by_asset_tag.get(tag).and_then(|id| self.entities.get(id))
    }

    /// Resolve a selector that may be an asset tag (e.g. `PUMP-01`) or a prim
    /// path (e.g. `/PumpStation01/PumpHall/Pump_01`).
    pub fn resolve_selector(&self, sel: &str) -> Option<&Entity> {
        if sel.starts_with('/') {
            self.by_path(sel)
        } else {
            self.by_asset_tag(sel).or_else(|| self.by_path(sel))
        }
    }

    pub fn entities(&self) -> impl Iterator<Item = &Entity> {
        self.entities.values()
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Total number of telemetry binding descriptors across all entities.
    pub fn binding_count(&self) -> usize {
        self.entities.values().map(|e| e.bindings.len()).sum()
    }

    /// Entities that carry at least one telemetry binding.
    pub fn entities_with_bindings(&self) -> usize {
        self.entities.values().filter(|e| !e.bindings.is_empty()).count()
    }

    /// Count of entities that reference a payload (deferred geometry).
    pub fn payload_count(&self) -> usize {
        self.entities.values().filter(|e| e.geom_ref.is_some()).count()
    }

    /// Entities that bind the given attribute (binding index lookup).
    pub fn entities_binding(&self, attribute: &str) -> Vec<EntityId> {
        self.binding_index.get(attribute).cloned().unwrap_or_default()
    }

    // ---- DSL patch mutators (spec §3.9) --------------------------------
    //
    // These apply Twin-Overlay opinions onto the LSG index *in place*, keeping
    // every `EntityId` stable so a DSL recompile patches rather than rebuilds.
    // None of them touch USD; the caller bumps `revision` once per patch via
    // [`Lsg::bump_revision`].

    /// Bump the index revision (call once after applying a batch of mutations).
    pub fn bump_revision(&mut self) {
        self.revision += 1;
    }

    /// Add a tag to an entity if absent. Returns true if the entity changed.
    pub fn add_tag(&mut self, id: EntityId, tag: &str) -> bool {
        match self.entities.get_mut(&id) {
            Some(e) if !e.tags.iter().any(|t| t == tag) => {
                e.tags.push(tag.to_string());
                true
            }
            _ => false,
        }
    }

    /// Remove a tag from an entity. Returns true if the entity changed.
    pub fn remove_tag(&mut self, id: EntityId, tag: &str) -> bool {
        match self.entities.get_mut(&id) {
            Some(e) => {
                let before = e.tags.len();
                e.tags.retain(|t| t != tag);
                before != e.tags.len()
            }
            None => false,
        }
    }

    /// Set a `customData.vf` metadata key on an entity (DSL `meta`).
    pub fn set_vf(&mut self, id: EntityId, key: &str, value: serde_json::Value) -> bool {
        match self.entities.get_mut(&id) {
            Some(e) => {
                e.vf.insert(key.to_string(), value);
                true
            }
            None => false,
        }
    }

    /// Remove a `customData.vf` metadata key from an entity.
    pub fn remove_vf(&mut self, id: EntityId, key: &str) -> bool {
        match self.entities.get_mut(&id) {
            Some(e) => e.vf.remove(key).is_some(),
            None => false,
        }
    }

    /// Add or replace a telemetry binding (matched by attribute), updating the
    /// binding index. Bindings stay declarative — never live values (spec §3.4).
    pub fn upsert_binding(&mut self, id: EntityId, binding: TelemetryBinding) -> bool {
        let Some(e) = self.entities.get_mut(&id) else {
            return false;
        };
        let attribute = binding.attribute.clone();
        let had = e.bindings.iter().any(|b| b.attribute == attribute);
        if let Some(existing) = e.bindings.iter_mut().find(|b| b.attribute == attribute) {
            *existing = binding;
        } else {
            e.bindings.push(binding);
        }
        if !had {
            self.binding_index
                .entry(attribute)
                .or_default()
                .push(id);
        }
        true
    }

    /// Remove a telemetry binding by attribute, updating the binding index.
    pub fn remove_binding(&mut self, id: EntityId, attribute: &str) -> bool {
        let Some(e) = self.entities.get_mut(&id) else {
            return false;
        };
        let before = e.bindings.len();
        e.bindings.retain(|b| b.attribute != attribute);
        let changed = before != e.bindings.len();
        if changed {
            if let Some(v) = self.binding_index.get_mut(attribute) {
                v.retain(|eid| *eid != id);
                if v.is_empty() {
                    self.binding_index.remove(attribute);
                }
            }
        }
        changed
    }

    /// Insert (or replace) an anchor by its stable id.
    pub fn insert_anchor(&mut self, anchor: Anchor) {
        self.anchors.insert(anchor.id, anchor);
    }

    pub fn remove_anchor(&mut self, id: AnchorId) -> bool {
        self.anchors.remove(&id).is_some()
    }

    /// Insert (or replace) an edge/pipe by its stable id.
    pub fn insert_edge(&mut self, edge: Edge) {
        self.edges.insert(edge.id, edge);
    }

    pub fn remove_edge(&mut self, id: EdgeId) -> bool {
        self.edges.remove(&id).is_some()
    }

    pub fn anchors(&self) -> impl Iterator<Item = &Anchor> {
        self.anchors.values()
    }

    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.values()
    }

    pub fn anchor_count(&self) -> usize {
        self.anchors.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_id_is_stable_and_deterministic() {
        let a = EntityId::from_prim_path("/PumpStation01/PumpHall/Pump_01");
        let b = EntityId::from_prim_path("/PumpStation01/PumpHall/Pump_01");
        assert_eq!(a, b);
        let c = EntityId::from_prim_path("/PumpStation01/PumpHall/Pump_02");
        assert_ne!(a, c);
    }

    #[test]
    fn transform_translation_roundtrip() {
        let t = Transform::from_translation([10.0, 6.0, -1.5]);
        assert_eq!(t.translation(), [10.0, 6.0, -1.5]);
    }
}

//! A fake renderer bridge (spec §3.5 / Phase 5 "Prove"): an in-repo,
//! engine-free consumer of the `vf.bridge.v1` stream that maintains a **Render
//! Scene as a cache** and reconstructs a scene purely from bridge messages plus
//! the fixture USD referenced by `GeomRef`s. It stands in for the O3DE Gem
//! (Phase 6) so the bridge protocol is provable without any engine.
//!
//! It obeys the spec §3.5 "Hard rule": it never invents durable entity IDs,
//! never persists pins locally as truth, and never treats a local USD stage as
//! authoritative for live twin state. On (re)connect it resyncs from the server
//! snapshot — its cache is disposable and rebuilt from `SnapshotBegin` + upserts.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use crate::bridge::{BridgeMsg, VisualSample};
use crate::hydrate::{HydratedPayload, PayloadCache};
use crate::lsg::{Aabb, EntityId, GeomRef, Transform};

/// One entity in the Render Scene cache. Everything here is a cache of the
/// logical world delivered over the bridge — never a source of truth.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderEntity {
    pub id: EntityId,
    pub prim_path: String,
    pub transform: Transform,
    /// Authored AABB relative to the transform origin (spec §3.6 proxy box).
    pub extents: Option<Aabb>,
    pub geom_ref: Option<GeomRef>,
    pub tags: Vec<String>,
    pub visual: Vec<VisualSample>,
    pub overlay_hint: Option<String>,
    /// Geometry hydrated from the fixture USD referenced by `geom_ref`. Filled
    /// by [`FakeBridge::hydrate`]; the raw bridge stream never carries meshes.
    pub hydrated: Option<Arc<HydratedPayload>>,
}

/// The fake bridge's Render Scene cache plus connection bookkeeping.
#[derive(Debug, Default)]
pub struct FakeBridge {
    scene: HashMap<EntityId, RenderEntity>,
    /// Negotiated protocol from the last `Hello`, if any.
    pub protocol: Option<String>,
    /// Scene revision from the last `Hello` / `SnapshotMarker`.
    pub scene_revision: u64,
    /// Last checkpoint `seq` seen (snapshot / diff marker).
    pub last_seq: u64,
    /// Last `PickResult` hit observed (for the CLI/tests).
    pub last_pick: Option<EntityId>,
    /// Whether a snapshot is currently open (between `SnapshotBegin` and marker).
    in_snapshot: bool,
}

impl FakeBridge {
    pub fn new() -> Self {
        FakeBridge::default()
    }

    /// Apply a batch of bridge messages to the Render Scene cache. A
    /// `SnapshotBegin` clears the scene so the following upserts rebuild it from
    /// scratch (the reconnect-resync path).
    pub fn apply(&mut self, msgs: &[BridgeMsg]) {
        for msg in msgs {
            self.apply_one(msg);
        }
    }

    fn apply_one(&mut self, msg: &BridgeMsg) {
        match msg {
            BridgeMsg::Hello {
                protocol,
                scene_revision,
            } => {
                self.protocol = Some(protocol.clone());
                self.scene_revision = *scene_revision;
            }
            BridgeMsg::SnapshotBegin { scene_revision, .. } => {
                // Full resync: drop the disposable cache and rebuild.
                self.scene.clear();
                self.scene_revision = *scene_revision;
                self.in_snapshot = true;
            }
            BridgeMsg::UpsertEntity {
                id,
                prim_path,
                transform,
                extents,
                geom_ref,
                tags,
                visual,
            } => {
                // Preserve any already-hydrated geometry if the geom ref is
                // unchanged, so a live diff upsert does not force a re-hydrate.
                let hydrated = match self.scene.get(id) {
                    Some(prev) if prev.geom_ref == *geom_ref => prev.hydrated.clone(),
                    _ => None,
                };
                self.scene.insert(
                    *id,
                    RenderEntity {
                        id: *id,
                        prim_path: prim_path.clone(),
                        transform: *transform,
                        extents: *extents,
                        geom_ref: geom_ref.clone(),
                        tags: tags.clone(),
                        visual: visual.clone(),
                        overlay_hint: None,
                        hydrated,
                    },
                );
            }
            BridgeMsg::RemoveEntity { id } => {
                self.scene.remove(id);
            }
            BridgeMsg::SetTransform { id, transform } => {
                if let Some(e) = self.scene.get_mut(id) {
                    e.transform = *transform;
                }
            }
            BridgeMsg::SetGeomRef { id, geom_ref } => {
                if let Some(e) = self.scene.get_mut(id) {
                    if e.geom_ref.as_ref() != Some(geom_ref) {
                        e.hydrated = None; // geometry changed; re-hydrate on demand
                    }
                    e.geom_ref = Some(geom_ref.clone());
                }
            }
            BridgeMsg::SetVisualState { id, visual } => {
                if let Some(e) = self.scene.get_mut(id) {
                    e.visual = visual.clone();
                }
            }
            BridgeMsg::SetOverlayHint { id, hint } => {
                if let Some(e) = self.scene.get_mut(id) {
                    e.overlay_hint = Some(hint.clone());
                }
            }
            BridgeMsg::SnapshotMarker {
                scene_revision,
                seq,
            } => {
                self.scene_revision = *scene_revision;
                self.last_seq = *seq;
                self.in_snapshot = false;
            }
            BridgeMsg::PinConfirm { id, transform, .. } => {
                // A confirmed pin is authoritative: reflect it in the cache, but
                // do NOT persist it locally as truth (it lives in the overlay).
                if let Some(e) = self.scene.get_mut(id) {
                    e.transform = *transform;
                }
            }
            BridgeMsg::PickResult { hit, .. } => {
                self.last_pick = *hit;
            }
        }
    }

    /// Hydrate geometry for every cached entity that references a payload and is
    /// not yet hydrated, resolving the `GeomRef` URI against the fixture USD via
    /// the payload cache. Returns the number of entities newly hydrated. Proves
    /// the bridge rebuilds geometry from diffs + fixture USD, never a live stage.
    pub fn hydrate(&mut self, cache: &mut PayloadCache) -> Result<usize> {
        let mut hydrated_now = 0;
        for e in self.scene.values_mut() {
            if e.hydrated.is_some() {
                continue;
            }
            if let Some(geom) = &e.geom_ref {
                let (_key, payload) = cache.acquire(geom)?;
                e.hydrated = Some(payload);
                hydrated_now += 1;
            }
        }
        Ok(hydrated_now)
    }

    pub fn get(&self, id: EntityId) -> Option<&RenderEntity> {
        self.scene.get(&id)
    }

    pub fn contains(&self, id: EntityId) -> bool {
        self.scene.contains_key(&id)
    }

    /// Number of entities in the Render Scene cache.
    pub fn len(&self) -> usize {
        self.scene.len()
    }

    pub fn is_empty(&self) -> bool {
        self.scene.is_empty()
    }

    /// Iterate over the Render Scene entities (order unspecified).
    pub fn entities(&self) -> impl Iterator<Item = &RenderEntity> {
        self.scene.values()
    }

    /// Sorted entity ids currently in the Render Scene (deterministic output).
    pub fn entity_ids(&self) -> Vec<EntityId> {
        let mut ids: Vec<EntityId> = self.scene.keys().copied().collect();
        ids.sort();
        ids
    }

    /// Count of entities that have been geometry-hydrated.
    pub fn hydrated_count(&self) -> usize {
        self.scene.values().filter(|e| e.hydrated.is_some()).count()
    }

    /// Disconnect: throw away the disposable Render Scene cache. Connection
    /// bookkeeping (protocol/revision) is cleared too so a reconnect starts
    /// from a fresh `Hello` + snapshot.
    pub fn disconnect(&mut self) {
        self.scene.clear();
        self.protocol = None;
        self.in_snapshot = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::PROTOCOL_VERSION;
    use crate::hydrate::StubPayloadLoader;

    fn geom() -> GeomRef {
        GeomRef {
            payload_uri: "./components/pump.usda".into(),
            prim_path: "/Pump".into(),
            content_hash: "h1".into(),
            lod_ladder: vec![],
        }
    }

    fn upsert(path: &str) -> BridgeMsg {
        BridgeMsg::UpsertEntity {
            id: EntityId::from_prim_path(path),
            prim_path: path.into(),
            transform: Transform::from_translation([1.0, 0.0, 0.0]),
            extents: Some(Aabb {
                min: [-0.5, -0.5, -0.5],
                max: [0.5, 0.5, 0.5],
            }),
            geom_ref: Some(geom()),
            tags: vec![],
            visual: vec![],
        }
    }

    #[test]
    fn snapshot_begin_clears_then_rebuilds() {
        let mut fb = FakeBridge::new();
        fb.apply(&[
            BridgeMsg::Hello {
                protocol: PROTOCOL_VERSION.into(),
                scene_revision: 1,
            },
            BridgeMsg::SnapshotBegin {
                subscription: 1,
                scene_revision: 1,
            },
            upsert("/a"),
            upsert("/b"),
            BridgeMsg::SnapshotMarker {
                scene_revision: 1,
                seq: 1,
            },
        ]);
        assert_eq!(fb.len(), 2);

        // A second snapshot replaces the whole scene.
        fb.apply(&[
            BridgeMsg::SnapshotBegin {
                subscription: 1,
                scene_revision: 2,
            },
            upsert("/c"),
            BridgeMsg::SnapshotMarker {
                scene_revision: 2,
                seq: 2,
            },
        ]);
        assert_eq!(fb.len(), 1);
        assert!(fb.contains(EntityId::from_prim_path("/c")));
        assert!(!fb.contains(EntityId::from_prim_path("/a")));
    }

    #[test]
    fn remove_and_set_transform() {
        let mut fb = FakeBridge::new();
        fb.apply(&[upsert("/a")]);
        let a = EntityId::from_prim_path("/a");
        fb.apply(&[BridgeMsg::SetTransform {
            id: a,
            transform: Transform::from_translation([9.0, 9.0, 9.0]),
        }]);
        assert_eq!(fb.get(a).unwrap().transform.translation(), [9.0, 9.0, 9.0]);
        fb.apply(&[BridgeMsg::RemoveEntity { id: a }]);
        assert!(fb.is_empty());
    }

    #[test]
    fn hydrate_fills_geometry_once() {
        let mut fb = FakeBridge::new();
        fb.apply(&[upsert("/a"), upsert("/b")]);
        let mut cache = PayloadCache::new(Box::new(StubPayloadLoader));
        let n = fb.hydrate(&mut cache).unwrap();
        assert_eq!(n, 2);
        assert_eq!(fb.hydrated_count(), 2);
        // Same shared payload content hash: only one distinct load.
        assert_eq!(cache.load_calls(), 1);
        // Idempotent: a second hydrate does nothing.
        assert_eq!(fb.hydrate(&mut cache).unwrap(), 0);
    }
}

//! Renderer Bridge API (`vf.bridge.v1`) — the engine-neutral seam between the
//! Scene Graph Service and any rendering backend (spec §3.5 / §5 / Phase 5).
//!
//! This module defines the **message schema** and the SGS-side [`BridgeServer`]
//! that encodes a Render Scene as a **snapshot + diff** stream and services
//! inbound bridge requests (camera/AOI, coarse pick, pin/unpin write-back). Per
//! spec §3.5 the contract is "the message schema + semantic versioning
//! (`vf.bridge.v1`), not the transport": the types here are `serde`-serializable
//! so they are wire-ready, but Phase 5 exchanges them over an in-process
//! `Vec<BridgeMsg>` batch. A real gRPC / WebTransport / shared-memory transport
//! is deferred to Phase 6–7 — "engine not required yet".
//!
//! Design rules enforced (spec §3.5 "Hard rule"):
//! - The bridge is a **cache of the logical world** — the server is the only
//!   authority for entity identity, topology, and pins.
//! - Transforms are resolved **pin > authored USD default** via the Twin
//!   Overlay ([`crate::overlay`]). The full §4.1 total order (authored <
//!   telemetry override < pin) enforcement lands in Phase 8; Phase 5 forwards
//!   resolved telemetry as visual state but does not yet let it drive the
//!   transform.
//! - Pick is **coarse** (ray vs AABB over the active set, spec §1.3 non-goal);
//!   bridges may refine with GPU picking later.
//! - This module never opens USD, never contacts VictoriaMetrics, and never
//!   writes the LSG; the only mutation it performs is a Twin-Overlay pin/unpin.

use serde::{Deserialize, Serialize};

use crate::interest::{Region, SubscriptionId};
use crate::lsg::{Aabb, Entity, EntityId, GeomRef, Lsg, Transform};
use crate::overlay::TwinOverlay;
use crate::rsg::{Rsg, RsgDiff};

/// The protocol version this SGS speaks. Bumped only on a breaking change; new
/// message variants are added additively first (spec §5 API versioning).
pub const PROTOCOL_VERSION: &str = "vf.bridge.v1";

/// Negotiate a protocol version against the versions a bridge offers on connect
/// (spec §5 "bridges negotiate on connect"). Returns the agreed version, or
/// `None` if the bridge speaks nothing we understand.
pub fn negotiate(offered: &[String]) -> Option<&'static str> {
    if offered.iter().any(|v| v == PROTOCOL_VERSION) {
        Some(PROTOCOL_VERSION)
    } else {
        None
    }
}

/// A resolved telemetry sample forwarded to the bridge as visual state (spec
/// §3.5 `SetVisualState`). The bridge maps `value`/`quality` to color/deform;
/// SGS does not decide final pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisualSample {
    pub attribute: String,
    pub value: f64,
    /// One of `ok` | `stale` | `unavailable` | `error` (spec §3.4).
    pub quality: String,
}

/// SGS → Bridge messages (spec §3.5 SGS→Bridge op table). Serialized with an
/// internal `type` tag so the wire form is self-describing and forward-additive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BridgeMsg {
    /// Sent once on connect with the negotiated protocol + current scene rev.
    Hello {
        protocol: String,
        scene_revision: u64,
    },
    /// Opens a full-state snapshot for a subscription; the bridge clears its
    /// Render Scene and rebuilds from the upserts that follow (spec §3.5
    /// reconnect resync).
    SnapshotBegin {
        subscription: SubscriptionId,
        scene_revision: u64,
    },
    /// Create or replace an entity in the Render Scene cache.
    UpsertEntity {
        id: EntityId,
        prim_path: String,
        transform: Transform,
        /// Authored AABB expressed **relative to the authored transform origin**
        /// (spec §3.6 WebGPU tier: the observer renders these as proxy boxes).
        /// Additive `vf.bridge.v1` field (2026-07 Transport amendment) — sending
        /// it origin-relative lets a pinned `transform` move the box without a
        /// protocol bump. `None` when the entity has no authored extents.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extents: Option<Aabb>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        geom_ref: Option<GeomRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        visual: Vec<VisualSample>,
    },
    /// Drop an entity from the Render Scene cache.
    RemoveEntity { id: EntityId },
    /// Live transform update (spec §3.5 `SetTransform`).
    SetTransform { id: EntityId, transform: Transform },
    /// Change the geometry handle / LOD (spec §3.5 `SetGeomRef/LOD`).
    SetGeomRef { id: EntityId, geom_ref: GeomRef },
    /// Telemetry-driven visual params (spec §3.5 `SetVisualState`).
    SetVisualState { id: EntityId, visual: Vec<VisualSample> },
    /// Non-geometry overlay annotation (spec §3.5 `SetOverlayHint`).
    SetOverlayHint { id: EntityId, hint: String },
    /// Closes a snapshot / marks a diff-stream checkpoint (spec §3.5
    /// `SnapshotMarker`). `seq` is monotonic within one server.
    SnapshotMarker { scene_revision: u64, seq: u64 },
    /// Acknowledges a `PinPart` / `UnpinPart`: the authoritative transform after
    /// the write-back plus the Twin-Overlay revision.
    PinConfirm {
        id: EntityId,
        transform: Transform,
        revision: u64,
    },
    /// Result of a coarse `PickRequest`.
    PickResult {
        request_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hit: Option<EntityId>,
    },
}

/// A spatial region on the wire (spec §3.2 `frustum | sphere | aoi_id`). Phase 5
/// carries the two serializable primitives; frustum wire form is deferred.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum RegionWire {
    Sphere { center: [f64; 3], radius: f64 },
    Aabb { min: [f64; 3], max: [f64; 3] },
}

impl RegionWire {
    /// Lower the wire region into the in-process interest [`Region`].
    pub fn to_region(&self) -> Region {
        match *self {
            RegionWire::Sphere { center, radius } => Region::Sphere { center, radius },
            RegionWire::Aabb { min, max } => Region::Aabb { min, max },
        }
    }
}

/// Bridge → SGS requests (spec §3.5 Bridge→SGS op table). The `Subscription`
/// these reshape is the same origin-agnostic struct Phase 2 builds from the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BridgeRequest {
    /// Connect handshake: the versions the bridge can speak.
    Connect { protocol_versions: Vec<String> },
    /// Camera moved / AOI reshaped (spec §3.5 `UpdateCamera` / `UpdateAOI`).
    UpdateAoi {
        subscription: SubscriptionId,
        region: RegionWire,
    },
    /// Coarse pick along a world-space ray (spec §3.5 `PickRequest`).
    PickRequest {
        request_id: u64,
        origin: [f64; 3],
        dir: [f64; 3],
    },
    /// Commit a transform pin (spec §3.5 `PinPart`); write-back to the overlay.
    PinPart { id: EntityId, transform: Transform },
    /// Remove a pin (spec §3.5 `UnpinPart`).
    UnpinPart { id: EntityId },
    /// Add explicit selection entities kept active regardless of the AOI.
    SubscribeExtras {
        subscription: SubscriptionId,
        entity_ids: Vec<EntityId>,
    },
    /// Liveness + budget (spec §3.5 heartbeat / budget).
    Heartbeat {
        subscription: SubscriptionId,
        budget: usize,
    },
    /// Request the tessellated mesh for a `GeomRef.content_hash` from the VF
    /// geometry store (spec §4.2 / Phase 5.6). The server answers with the GLB
    /// bytes as a binary WebSocket frame (empty frame when absent/unknown, so
    /// the observer keeps its LOD-0 proxy box). Additive — still `vf.bridge.v1`.
    FetchGeom { content_hash: String },
}

/// SGS-side encoder for one bridge connection. Stateless apart from a monotonic
/// `seq` used to stamp snapshot / diff checkpoints. All scene state is read from
/// the LSG (defaults), the RSG (active set + telemetry), and the Twin Overlay
/// (pins) that are passed in explicitly — the server owns none of it.
#[derive(Debug, Default)]
pub struct BridgeServer {
    seq: u64,
}

impl BridgeServer {
    pub fn new() -> Self {
        BridgeServer { seq: 0 }
    }

    /// The negotiated `Hello` for a freshly-connected bridge.
    pub fn hello(&self, lsg: &Lsg) -> BridgeMsg {
        BridgeMsg::Hello {
            protocol: PROTOCOL_VERSION.to_string(),
            scene_revision: lsg.revision(),
        }
    }

    /// Build a full-state snapshot for a subscription: `SnapshotBegin`, one
    /// `UpsertEntity` per active entity (transform resolved pin > authored,
    /// geom ref + telemetry attached), then a closing `SnapshotMarker`. This is
    /// what a bridge replays on connect / reconnect to rebuild its cache.
    pub fn snapshot(
        &mut self,
        subscription: SubscriptionId,
        lsg: &Lsg,
        rsg: &Rsg,
        overlay: &TwinOverlay,
    ) -> anyhow::Result<Vec<BridgeMsg>> {
        let mut msgs = Vec::new();
        msgs.push(BridgeMsg::SnapshotBegin {
            subscription,
            scene_revision: lsg.revision(),
        });

        // Deterministic order so snapshots are byte-stable / golden-testable.
        let mut ids: Vec<EntityId> = rsg
            .entities()
            .filter(|re| re.subscribers.contains(&subscription))
            .map(|re| re.id)
            .collect();
        ids.sort();

        for id in ids {
            if let Some(msg) = self.upsert_for(id, lsg, rsg, overlay)? {
                msgs.push(msg);
            }
        }

        self.seq += 1;
        msgs.push(BridgeMsg::SnapshotMarker {
            scene_revision: lsg.revision(),
            seq: self.seq,
        });
        Ok(msgs)
    }

    /// Encode one subscription's [`RsgDiff`] as bridge messages: upserts become
    /// `UpsertEntity` (full resolved state), removes become `RemoveEntity`.
    pub fn encode_diff(
        &mut self,
        // The diff is already scoped to one subscriber's cursor; the id is taken
        // for call-site parity with `snapshot` and future per-sub policy.
        _subscription: SubscriptionId,
        diff: &RsgDiff,
        lsg: &Lsg,
        rsg: &Rsg,
        overlay: &TwinOverlay,
    ) -> anyhow::Result<Vec<BridgeMsg>> {
        let mut msgs = Vec::new();
        for &id in &diff.upserts {
            if let Some(msg) = self.upsert_for(id, lsg, rsg, overlay)? {
                msgs.push(msg);
            }
        }
        for &id in &diff.removes {
            msgs.push(BridgeMsg::RemoveEntity { id });
        }
        Ok(msgs)
    }

    /// Build an `UpsertEntity` for `id` from LSG defaults + overlay pin + RSG
    /// telemetry. Returns `None` if the entity is not in the LSG index.
    fn upsert_for(
        &self,
        id: EntityId,
        lsg: &Lsg,
        rsg: &Rsg,
        overlay: &TwinOverlay,
    ) -> anyhow::Result<Option<BridgeMsg>> {
        let Some(entity) = lsg.get(id) else {
            return Ok(None);
        };
        // pin > authored (spec §4.1); telemetry-driven transform is Phase 8.
        let transform = overlay.resolved_transform(entity)?;
        let visual = rsg
            .get(id)
            .map(collect_visual)
            .unwrap_or_default();
        Ok(Some(BridgeMsg::UpsertEntity {
            id,
            prim_path: entity.prim_path.clone(),
            transform,
            extents: extents_relative_to_authored(entity),
            geom_ref: entity.geom_ref.clone(),
            tags: entity.tags.clone(),
            visual,
        }))
    }

    /// Handle a `PinPart`: write the override through to the Twin Overlay
    /// (durable, zero USD writes) and return the authoritative `PinConfirm`.
    pub fn handle_pin(
        &self,
        id: EntityId,
        prim_path: &str,
        transform: Transform,
        pinned_by: Option<&str>,
        overlay: &mut TwinOverlay,
    ) -> anyhow::Result<BridgeMsg> {
        let revision = overlay.pin(id, prim_path, transform, pinned_by)?;
        Ok(BridgeMsg::PinConfirm {
            id,
            transform,
            revision,
        })
    }

    /// Handle an `UnpinPart`: drop the override; the entity reverts to its
    /// authored default, echoed back in the `PinConfirm`.
    pub fn handle_unpin(
        &self,
        id: EntityId,
        authored: Transform,
        overlay: &mut TwinOverlay,
    ) -> anyhow::Result<BridgeMsg> {
        let (revision, _existed) = overlay.unpin(id)?;
        Ok(BridgeMsg::PinConfirm {
            id,
            transform: authored,
            revision,
        })
    }

    /// Coarse pick: intersect a world-space ray against the AABB extents of the
    /// entities active for `subscription`, returning the nearest hit (spec §1.3
    /// coarse-pick non-goal; bridges may refine with GPU picking). Uses authored
    /// LSG extents — coarse by design.
    pub fn coarse_pick(
        &self,
        subscription: SubscriptionId,
        request_id: u64,
        origin: [f64; 3],
        dir: [f64; 3],
        lsg: &Lsg,
        rsg: &Rsg,
    ) -> BridgeMsg {
        let mut best: Option<(f64, EntityId)> = None;
        for re in rsg
            .entities()
            .filter(|re| re.subscribers.contains(&subscription))
        {
            let Some(entity) = lsg.get(re.id) else { continue };
            if let Some(t) = ray_aabb(origin, dir, &entity.extents) {
                match best {
                    Some((bt, bid)) if bt < t || (bt == t && bid <= re.id) => {}
                    _ => best = Some((t, re.id)),
                }
            }
        }
        BridgeMsg::PickResult {
            request_id,
            hit: best.map(|(_, id)| id),
        }
    }

    /// Current checkpoint sequence (diagnostics).
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

/// Re-express an entity's authored world-space `extents` relative to its
/// authored transform origin, so the observer client can anchor the proxy box
/// at `transform.translation + extents` — meaning a pin that moves `transform`
/// moves the box, while an unpinned entity renders at its original world AABB.
/// Returns `None` for a degenerate (zero) authored box.
fn extents_relative_to_authored(entity: &Entity) -> Option<Aabb> {
    let ext = &entity.extents;
    if ext.min == [0.0; 3] && ext.max == [0.0; 3] {
        return None;
    }
    let o = entity.transform_default.translation();
    Some(Aabb {
        min: [ext.min[0] - o[0], ext.min[1] - o[1], ext.min[2] - o[2]],
        max: [ext.max[0] - o[0], ext.max[1] - o[1], ext.max[2] - o[2]],
    })
}

/// Gather the RSG telemetry cache for an entity into wire visual samples.
fn collect_visual(re: &crate::rsg::RuntimeEntity) -> Vec<VisualSample> {
    let mut out: Vec<VisualSample> = re
        .telemetry
        .iter()
        .map(|(attr, v)| VisualSample {
            attribute: attr.clone(),
            value: v.value,
            quality: v.quality.as_str().to_string(),
        })
        .collect();
    out.sort_by(|a, b| a.attribute.cmp(&b.attribute));
    out
}

/// Slab-method ray vs AABB. Returns the entry distance `t >= 0` if the ray
/// (origin + t*dir) intersects the box, else `None`. A degenerate/zero
/// direction component is treated as "must already be inside that slab".
pub fn ray_aabb(origin: [f64; 3], dir: [f64; 3], aabb: &Aabb) -> Option<f64> {
    let mut tmin = f64::NEG_INFINITY;
    let mut tmax = f64::INFINITY;
    for i in 0..3 {
        if dir[i].abs() < 1e-9 {
            if origin[i] < aabb.min[i] || origin[i] > aabb.max[i] {
                return None;
            }
        } else {
            let inv = 1.0 / dir[i];
            let mut t1 = (aabb.min[i] - origin[i]) * inv;
            let mut t2 = (aabb.max[i] - origin[i]) * inv;
            if t1 > t2 {
                std::mem::swap(&mut t1, &mut t2);
            }
            tmin = tmin.max(t1);
            tmax = tmax.min(t2);
            if tmin > tmax {
                return None;
            }
        }
    }
    if tmax < 0.0 {
        return None; // box is entirely behind the ray origin
    }
    Some(tmin.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_accepts_v1_rejects_unknown() {
        assert_eq!(negotiate(&["vf.bridge.v1".into()]), Some("vf.bridge.v1"));
        assert_eq!(
            negotiate(&["vf.bridge.v0".into(), "vf.bridge.v1".into()]),
            Some("vf.bridge.v1")
        );
        assert_eq!(negotiate(&["vf.bridge.v2".into()]), None);
        assert_eq!(negotiate(&[]), None);
    }

    #[test]
    fn bridge_msg_json_round_trip() {
        let msgs = vec![
            BridgeMsg::Hello {
                protocol: PROTOCOL_VERSION.into(),
                scene_revision: 7,
            },
            BridgeMsg::UpsertEntity {
                id: EntityId::from_prim_path("/a"),
                prim_path: "/a".into(),
                transform: Transform::from_translation([1.0, 2.0, 3.0]),
                extents: Some(Aabb {
                    min: [-0.5, -0.5, -0.5],
                    max: [0.5, 0.5, 0.5],
                }),
                geom_ref: Some(GeomRef {
                    payload_uri: "./components/pump.usda".into(),
                    prim_path: "/Pump".into(),
                    content_hash: "abc".into(),
                    lod_ladder: vec![],
                }),
                tags: vec!["pump".into()],
                visual: vec![VisualSample {
                    attribute: "flow".into(),
                    value: 42.0,
                    quality: "ok".into(),
                }],
            },
            BridgeMsg::RemoveEntity {
                id: EntityId::from_prim_path("/b"),
            },
            BridgeMsg::SnapshotMarker {
                scene_revision: 7,
                seq: 1,
            },
            BridgeMsg::PickResult {
                request_id: 9,
                hit: Some(EntityId::from_prim_path("/a")),
            },
        ];
        let json = serde_json::to_string(&msgs).unwrap();
        let back: Vec<BridgeMsg> = serde_json::from_str(&json).unwrap();
        assert_eq!(msgs, back);
    }

    #[test]
    fn bridge_request_json_round_trip() {
        let reqs = vec![
            BridgeRequest::Connect {
                protocol_versions: vec!["vf.bridge.v1".into()],
            },
            BridgeRequest::UpdateAoi {
                subscription: 1,
                region: RegionWire::Sphere {
                    center: [0.0, 3.0, 0.0],
                    radius: 8.0,
                },
            },
            BridgeRequest::PickRequest {
                request_id: 5,
                origin: [0.0, 0.0, -10.0],
                dir: [0.0, 0.0, 1.0],
            },
            BridgeRequest::PinPart {
                id: EntityId::from_prim_path("/a"),
                transform: Transform::identity(),
            },
            BridgeRequest::FetchGeom {
                content_hash: "abc123".into(),
            },
        ];
        let json = serde_json::to_string(&reqs).unwrap();
        let back: Vec<BridgeRequest> = serde_json::from_str(&json).unwrap();
        assert_eq!(reqs, back);
    }

    #[test]
    fn ray_aabb_hits_front_box_and_misses_offset() {
        let box_ = Aabb {
            min: [-1.0, -1.0, -1.0],
            max: [1.0, 1.0, 1.0],
        };
        // Ray down +z from behind hits the front face at t=9.
        let t = ray_aabb([0.0, 0.0, -10.0], [0.0, 0.0, 1.0], &box_).unwrap();
        assert!((t - 9.0).abs() < 1e-6, "t={t}");
        // Parallel ray offset in x misses.
        assert!(ray_aabb([5.0, 0.0, -10.0], [0.0, 0.0, 1.0], &box_).is_none());
        // Box entirely behind the origin: miss.
        assert!(ray_aabb([0.0, 0.0, 10.0], [0.0, 0.0, 1.0], &box_).is_none());
    }
}

// vf.bridge.v1 — TypeScript mirror of the Rust `serde` schema in
// sgs/src/bridge.rs (spec §3.5 / §5). SGS -> Bridge messages are `BridgeMsg`;
// Bridge -> SGS requests are `BridgeRequest`. The server frames a batch as a
// single JSON array text frame; this client applies `BridgeMsg[]`.
//
// EntityId is a 16-char hex STRING on the wire (a full 64-bit id overflows a JS
// safe integer) — see sgs/src/lsg.rs. Keep it opaque: use it as a Map key and
// echo it back verbatim on pick/pin so the server matches it exactly.

export const PROTOCOL_VERSION = "vf.bridge.v1";

export type EntityId = string;

/** Row-major 4x4 transform (spec §4.2). */
export interface Transform {
  matrix: number[]; // length 16
}

/** Axis-aligned bounding box (spec §3.1). On the wire in `UpsertEntity` it is
 *  authored extents expressed relative to the transform origin. */
export interface Aabb {
  min: [number, number, number];
  max: [number, number, number];
}

export interface GeomRef {
  payload_uri: string;
  prim_path: string;
  content_hash: string;
  lod_ladder: string[];
}

/** A resolved telemetry sample forwarded as visual state (spec §3.5). */
export interface VisualSample {
  attribute: string;
  value: number;
  quality: string; // "ok" | "stale" | "unavailable" | "error"
}

// ---- SGS -> Bridge ----------------------------------------------------------

export type BridgeMsg =
  | { type: "Hello"; protocol: string; scene_revision: number }
  | { type: "SnapshotBegin"; subscription: number; scene_revision: number }
  | {
      type: "UpsertEntity";
      id: EntityId;
      prim_path: string;
      transform: Transform;
      extents?: Aabb;
      geom_ref?: GeomRef;
      tags?: string[];
      visual?: VisualSample[];
    }
  | { type: "RemoveEntity"; id: EntityId }
  | { type: "SetTransform"; id: EntityId; transform: Transform }
  | { type: "SetGeomRef"; id: EntityId; geom_ref: GeomRef }
  | { type: "SetVisualState"; id: EntityId; visual: VisualSample[] }
  | { type: "SetOverlayHint"; id: EntityId; hint: string }
  | { type: "SnapshotMarker"; scene_revision: number; seq: number }
  | { type: "PinConfirm"; id: EntityId; transform: Transform; revision: number }
  | { type: "PickResult"; request_id: number; hit?: EntityId };

// ---- Bridge -> SGS ----------------------------------------------------------

export type RegionWire =
  | { shape: "sphere"; center: [number, number, number]; radius: number }
  | { shape: "aabb"; min: [number, number, number]; max: [number, number, number] };

export type BridgeRequest =
  | { type: "Connect"; protocol_versions: string[] }
  | { type: "UpdateAoi"; subscription: number; region: RegionWire }
  | {
      type: "PickRequest";
      request_id: number;
      origin: [number, number, number];
      dir: [number, number, number];
    }
  | { type: "PinPart"; id: EntityId; transform: Transform }
  | { type: "UnpinPart"; id: EntityId }
  | { type: "SubscribeExtras"; subscription: number; entity_ids: EntityId[] }
  | { type: "Heartbeat"; subscription: number; budget: number }
  // Phase 5.6: request a tessellated mesh by GeomRef.content_hash from the VF
  // geometry store. The server answers with GLB bytes as a binary frame (an
  // empty frame means "absent" — the client keeps its proxy box).
  | { type: "FetchGeom"; content_hash: string };

/** A decoded GLB mesh ready for GPU upload. */
export interface MeshData {
  positions: Float32Array; // xyz triples
  normals: Float32Array; // xyz triples
  indices: Uint32Array;
}

/** Minimal GLB (binary glTF 2.0) parser matching the SGS geomstore writer:
 *  a single mesh primitive with POSITION + NORMAL + SCALAR indices. Returns
 *  null for an empty/absent frame or on any parse failure (keep the box). */
export function parseGlb(buf: ArrayBuffer): MeshData | null {
  if (buf.byteLength < 12) return null;
  const dv = new DataView(buf);
  if (dv.getUint32(0, true) !== 0x46546c67) return null; // "glTF"

  let off = 12;
  let json: unknown = null;
  let bin: Uint8Array | null = null;
  while (off + 8 <= buf.byteLength) {
    const len = dv.getUint32(off, true);
    const kind = dv.getUint32(off + 4, true);
    const start = off + 8;
    if (start + len > buf.byteLength) break;
    if (kind === 0x4e4f534a) {
      json = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, start, len)));
    } else if (kind === 0x004e4942) {
      bin = new Uint8Array(buf, start, len);
    }
    off = start + len;
  }
  if (!json || !bin) return null;
  const doc = json as any;

  const accessors = doc.accessors ?? [];
  const views = doc.bufferViews ?? [];
  const prim = doc.meshes?.[0]?.primitives?.[0];
  if (!prim) return null;

  const vec3 = (accIdx: number): Float32Array => {
    const acc = accessors[accIdx];
    const count = acc.count ?? 0;
    const view = views[acc.bufferView];
    const base = (view.byteOffset ?? 0) + bin!.byteOffset;
    return new Float32Array(bin!.buffer.slice(base, base + count * 12));
  };

  const posIdx = prim.attributes?.POSITION;
  const idxAcc = prim.indices;
  if (posIdx === undefined || idxAcc === undefined) return null;
  const positions = vec3(posIdx);
  const normals = prim.attributes?.NORMAL !== undefined ? vec3(prim.attributes.NORMAL) : new Float32Array(positions.length);

  const ia = accessors[idxAcc];
  const iv = views[ia.bufferView];
  const ibase = (iv.byteOffset ?? 0) + bin.byteOffset;
  const indices = new Uint32Array(bin.buffer.slice(ibase, ibase + (ia.count ?? 0) * 4));

  if (positions.length === 0 || indices.length === 0) return null;
  return { positions, normals, indices };
}

/** Identity transform helper (matches Rust `Transform::identity`). */
export function identityTransform(): Transform {
  const m = new Array(16).fill(0);
  m[0] = 1;
  m[5] = 1;
  m[10] = 1;
  m[15] = 1;
  return { matrix: m };
}

/** Translation-only transform (row-major, matches Rust `from_translation`). */
export function translationTransform(t: [number, number, number]): Transform {
  const tf = identityTransform();
  tf.matrix[3] = t[0];
  tf.matrix[7] = t[1];
  tf.matrix[11] = t[2];
  return tf;
}

/** Extract the translation column (rows 0..3 of the last column). */
export function translationOf(tf: Transform): [number, number, number] {
  return [tf.matrix[3], tf.matrix[7], tf.matrix[11]];
}

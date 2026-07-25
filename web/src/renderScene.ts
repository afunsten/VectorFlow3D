// RenderScene — the browser's disposable Render Scene cache, a direct port of
// the Rust `FakeBridge` (sgs/src/fake_bridge.rs). It reconstructs the active set
// purely from the `vf.bridge.v1` message stream and obeys the spec §3.5 hard
// rule: it invents no ids, persists no pins as truth, and rebuilds identically
// from a fresh `SnapshotBegin` + upserts on reconnect. This is a cache of the
// logical world delivered over the bridge — never a source of truth.

import type { Aabb, BridgeMsg, EntityId, GeomRef, Transform, VisualSample } from "./protocol";

/** One entity in the Render Scene cache (mirror of Rust `RenderEntity`). */
export interface RenderEntity {
  id: EntityId;
  primPath: string;
  transform: Transform;
  extents?: Aabb;
  geomRef?: GeomRef;
  tags: string[];
  visual: VisualSample[];
  overlayHint?: string;
}

export class RenderScene {
  private scene = new Map<EntityId, RenderEntity>();

  /** Negotiated protocol from the last Hello. */
  protocol: string | null = null;
  /** Scene revision from the last Hello / SnapshotMarker. */
  sceneRevision = 0;
  /** Last checkpoint seq seen. */
  lastSeq = 0;
  /** Last PickResult hit observed. */
  lastPick: EntityId | null = null;
  private inSnapshot = false;

  /** Apply a batch of bridge messages to the cache. */
  apply(msgs: BridgeMsg[]): void {
    for (const m of msgs) this.applyOne(m);
  }

  private applyOne(msg: BridgeMsg): void {
    switch (msg.type) {
      case "Hello":
        this.protocol = msg.protocol;
        this.sceneRevision = msg.scene_revision;
        break;
      case "SnapshotBegin":
        // Full resync: drop the disposable cache and rebuild from the upserts.
        this.scene.clear();
        this.sceneRevision = msg.scene_revision;
        this.inSnapshot = true;
        break;
      case "UpsertEntity":
        this.scene.set(msg.id, {
          id: msg.id,
          primPath: msg.prim_path,
          transform: msg.transform,
          extents: msg.extents,
          geomRef: msg.geom_ref,
          tags: msg.tags ?? [],
          visual: msg.visual ?? [],
          overlayHint: this.scene.get(msg.id)?.overlayHint,
        });
        break;
      case "RemoveEntity":
        this.scene.delete(msg.id);
        break;
      case "SetTransform": {
        const e = this.scene.get(msg.id);
        if (e) e.transform = msg.transform;
        break;
      }
      case "SetGeomRef": {
        const e = this.scene.get(msg.id);
        if (e) e.geomRef = msg.geom_ref;
        break;
      }
      case "SetVisualState": {
        const e = this.scene.get(msg.id);
        if (e) e.visual = msg.visual;
        break;
      }
      case "SetOverlayHint": {
        const e = this.scene.get(msg.id);
        if (e) e.overlayHint = msg.hint;
        break;
      }
      case "SnapshotMarker":
        this.sceneRevision = msg.scene_revision;
        this.lastSeq = msg.seq;
        this.inSnapshot = false;
        break;
      case "PinConfirm": {
        // A confirmed pin is authoritative: reflect it, but do NOT persist it
        // locally as truth (it lives in the Twin Overlay).
        const e = this.scene.get(msg.id);
        if (e) e.transform = msg.transform;
        break;
      }
      case "PickResult":
        this.lastPick = msg.hit ?? null;
        break;
    }
  }

  get(id: EntityId): RenderEntity | undefined {
    return this.scene.get(id);
  }

  has(id: EntityId): boolean {
    return this.scene.has(id);
  }

  get size(): number {
    return this.scene.size;
  }

  entities(): IterableIterator<RenderEntity> {
    return this.scene.values();
  }

  /** Sorted entity ids (deterministic; matches Rust `entity_ids`). */
  entityIds(): EntityId[] {
    return [...this.scene.keys()].sort();
  }

  isSnapshotOpen(): boolean {
    return this.inSnapshot;
  }

  /** Disconnect: throw away the disposable cache so a reconnect rebuilds from a
   *  fresh Hello + snapshot. */
  disconnect(): void {
    this.scene.clear();
    this.protocol = null;
    this.inSnapshot = false;
  }
}

// Observer client bootstrap: connect to `sgs serve` over WebSocket, apply the
// vf.bridge.v1 stream into the RenderScene cache, draw the active set as AABB
// proxy boxes, and send camera/pick/pin back. Reloading the page reconnects and
// rebuilds the scene identically from a fresh snapshot (spec §3.5).

import { OrbitCamera } from "./camera";
import { Renderer } from "./renderer";
import { RenderScene } from "./renderScene";
import type { BridgeMsg, BridgeRequest, EntityId } from "./protocol";
import { PROTOCOL_VERSION, parseGlb, translationOf, translationTransform } from "./protocol";

const SUB = 1;

const canvas = document.getElementById("gpu") as HTMLCanvasElement;
const hud = document.getElementById("hud") as HTMLDivElement;
const wsInput = document.getElementById("ws") as HTMLInputElement;
const statusEl = document.getElementById("status") as HTMLSpanElement;
const countEl = document.getElementById("count") as HTMLSpanElement;
const revEl = document.getElementById("rev") as HTMLSpanElement;
const selEl = document.getElementById("sel") as HTMLSpanElement;
const errEl = document.getElementById("err") as HTMLDivElement;

function defaultEndpoint(): string {
  const q = new URLSearchParams(location.search).get("ws");
  return q ?? "ws://127.0.0.1:8787";
}

function showError(msg: string): void {
  errEl.textContent = msg;
  errEl.style.display = "block";
}

const scene = new RenderScene();
const camera = new OrbitCamera();
let selected: EntityId | null = null;
let pickSeq = 1;

async function main(): Promise<void> {
  const renderer = new Renderer(canvas);
  try {
    await renderer.init();
  } catch (e) {
    showError(`WebGPU init failed: ${(e as Error).message}\nUse a WebGPU-capable browser (Chrome/Edge 113+, Safari 18+).`);
    return;
  }

  wsInput.value = defaultEndpoint();
  const client = new BridgeClient(scene, camera, renderer);
  client.connect(wsInput.value);
  wsInput.addEventListener("change", () => client.connect(wsInput.value));

  wireInput(renderer, client);

  const frame = () => {
    // Reflect the picked entity from the last PickResult.
    if (scene.lastPick !== undefined) selected = scene.lastPick;
    renderer.render(scene, camera, selected);
    countEl.textContent = String(scene.size);
    revEl.textContent = String(scene.sceneRevision);
    selEl.textContent = selected ? selected.slice(0, 12) : "—";
    hud.classList.toggle("connected", client.isOpen());
    statusEl.textContent = client.status;
    requestAnimationFrame(frame);
  };
  requestAnimationFrame(frame);
}

function wireInput(renderer: Renderer, client: BridgeClient): void {
  let dragging = false;
  let panning = false;
  let moved = 0;
  let lastX = 0;
  let lastY = 0;

  canvas.addEventListener("pointerdown", (e) => {
    dragging = true;
    panning = e.button === 2 || e.shiftKey;
    moved = 0;
    lastX = e.clientX;
    lastY = e.clientY;
    canvas.setPointerCapture(e.pointerId);
  });
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());

  canvas.addEventListener("pointermove", (e) => {
    if (!dragging) return;
    const dx = e.clientX - lastX;
    const dy = e.clientY - lastY;
    lastX = e.clientX;
    lastY = e.clientY;
    moved += Math.abs(dx) + Math.abs(dy);
    if (panning) {
      camera.pan(dx, dy);
      client.scheduleAoi();
    } else {
      camera.orbit(dx, dy);
    }
  });

  canvas.addEventListener("pointerup", (e) => {
    dragging = false;
    canvas.releasePointerCapture(e.pointerId);
    if (moved < 5) {
      // Treat as a click -> coarse pick along the world ray.
      const rect = canvas.getBoundingClientRect();
      const ndcX = ((e.clientX - rect.left) / rect.width) * 2 - 1;
      const ndcY = -(((e.clientY - rect.top) / rect.height) * 2 - 1);
      const ray = camera.rayFromNdc(ndcX, ndcY, renderer.aspect);
      client.send({ type: "PickRequest", request_id: pickSeq++, origin: ray.origin, dir: ray.dir });
    }
  });

  canvas.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      camera.zoom(e.deltaY);
      // AOI radius grows with distance so zoom changes the active set.
      camera.aoiRadius = Math.max(4, Math.min(400, camera.distance * 0.55));
      client.scheduleAoi();
    },
    { passive: false },
  );

  window.addEventListener("keydown", (e) => {
    const step = 40;
    switch (e.key.toLowerCase()) {
      case "w":
      case "arrowup":
        camera.pan(0, step);
        client.scheduleAoi();
        break;
      case "s":
      case "arrowdown":
        camera.pan(0, -step);
        client.scheduleAoi();
        break;
      case "a":
      case "arrowleft":
        camera.pan(step, 0);
        client.scheduleAoi();
        break;
      case "d":
      case "arrowright":
        camera.pan(-step, 0);
        client.scheduleAoi();
        break;
      case "p":
        pinSelected(client);
        break;
      case "u":
        if (selected) client.send({ type: "UnpinPart", id: selected });
        break;
    }
  });

  window.addEventListener("resize", () => renderer.resize());
}

function pinSelected(client: BridgeClient): void {
  if (!selected) return;
  const e = scene.get(selected);
  if (!e) return;
  const t = translationOf(e.transform);
  // Raise the pinned box so the move is visible; server persists it in the
  // Twin Overlay and echoes a PinConfirm the box snaps to.
  const pinned = translationTransform([t[0], t[1], t[2] + 8]);
  client.send({ type: "PinPart", id: selected, transform: pinned });
}

/** Owns the WebSocket, applies frames to the scene, and reconnects on drop. */
class BridgeClient {
  private ws: WebSocket | null = null;
  private endpoint = "";
  private aoiTimer: number | null = null;
  private heartbeat: number | null = null;
  // Phase 5.6 geometry fetch: hashes already requested, and a FIFO of the
  // outstanding requests so incoming binary frames pair with their hash (the
  // server answers one binary frame per FetchGeom, in order, per connection).
  private requested = new Set<string>();
  private fifo: string[] = [];
  status = "connecting…";

  constructor(
    private scene: RenderScene,
    private camera: OrbitCamera,
    private renderer: Renderer,
  ) {}

  isOpen(): boolean {
    return this.ws?.readyState === WebSocket.OPEN;
  }

  connect(endpoint: string): void {
    this.endpoint = endpoint;
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.close();
    }
    this.scene.disconnect();
    // Drop geometry-fetch state + uploaded meshes; a fresh snapshot re-fetches
    // by hash and the server-side store dedups (no redundant tessellation).
    this.requested.clear();
    this.fifo.length = 0;
    this.renderer.clearMeshes();
    this.status = "connecting…";
    let ws: WebSocket;
    try {
      ws = new WebSocket(endpoint);
    } catch (e) {
      this.status = `bad endpoint: ${(e as Error).message}`;
      return;
    }
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = () => {
      this.status = "connected";
      this.send({ type: "Connect", protocol_versions: [PROTOCOL_VERSION] });
      if (this.heartbeat) clearInterval(this.heartbeat);
      this.heartbeat = setInterval(() => this.send({ type: "Heartbeat", subscription: SUB, budget: 0 }), 5000) as unknown as number;
    };
    ws.onmessage = (ev) => {
      // Binary frames carry GLB mesh bytes (Phase 5.6), paired to the oldest
      // outstanding FetchGeom by FIFO order. An empty frame means "absent".
      if (ev.data instanceof ArrayBuffer) {
        const hash = this.fifo.shift();
        if (!hash) return;
        const mesh = parseGlb(ev.data);
        if (mesh) this.renderer.setMesh(hash, mesh);
        return;
      }
      let batch: BridgeMsg[];
      try {
        batch = JSON.parse(ev.data as string);
      } catch {
        return;
      }
      this.scene.apply(batch);
      // After the server's Hello, reshape the AOI to this client's camera.
      if (batch.some((m) => m.type === "Hello")) this.sendAoi();
      // Fetch any newly-referenced meshes by content hash (deduped).
      this.requestMeshes();
    };
    ws.onerror = () => {
      this.status = "error";
    };
    ws.onclose = () => {
      this.status = "disconnected — retrying…";
      if (this.heartbeat) clearInterval(this.heartbeat);
      setTimeout(() => {
        if (this.endpoint === endpoint) this.connect(endpoint);
      }, 1200);
    };
  }

  send(req: BridgeRequest): void {
    if (this.isOpen()) this.ws!.send(JSON.stringify(req));
  }

  /** Request a GLB for every unique, not-yet-fetched content hash in the scene
   *  (Phase 5.6). Deduped by `requested` so a mesh is fetched at most once per
   *  connection; the proxy box renders until the mesh arrives. */
  private requestMeshes(): void {
    for (const e of this.scene.entities()) {
      const hash = e.geomRef?.content_hash;
      if (!hash || this.requested.has(hash) || this.renderer.hasMesh(hash)) continue;
      this.requested.add(hash);
      this.fifo.push(hash);
      this.send({ type: "FetchGeom", content_hash: hash });
    }
  }

  /** Debounced AOI update so a drag/zoom does not flood the server. */
  scheduleAoi(): void {
    if (this.aoiTimer) return;
    this.aoiTimer = setTimeout(() => {
      this.aoiTimer = null;
      this.sendAoi();
    }, 80) as unknown as number;
  }

  private sendAoi(): void {
    const c = this.camera.aoiCenter();
    this.send({
      type: "UpdateAoi",
      subscription: SUB,
      region: { shape: "sphere", center: [c[0], c[1], c[2]], radius: this.camera.aoiRadius },
    });
  }
}

void main();

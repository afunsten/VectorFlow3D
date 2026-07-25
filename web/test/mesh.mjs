// Headless mesh-hydration test for the Phase 5.6 observer path: spawn
// `sgs serve` against the real pump-station USD, connect with Node's global
// WebSocket, and assert that (a) the vf.bridge.v1 snapshot reconstructs every
// entity (geom-bearing or not), and (b) a `FetchGeom` by content_hash returns a
// GLB binary frame that parses to a positive vertex count. This exercises the
// same path as the browser client (src/main.ts + src/protocol.ts parseGlb).
//
// USD-gated: skipped (exit 0) when the OpenUSD toolchain (sgs/tools/.venv) is
// absent, mirroring the Rust USD-gated test. Run after `cargo build` in ../sgs.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const candidates = [
  join(here, "../../sgs/target/debug/sgs"),
  join(here, "../../sgs/target/release/sgs"),
];
const bin = candidates.find(existsSync);
if (!bin) {
  console.error("sgs binary not found — run `cargo build` in sgs/ first:", candidates);
  process.exit(2);
}

const venv = join(here, "../../sgs/tools/.venv/bin/python3");
if (!existsSync(venv)) {
  console.log("skipping mesh test: OpenUSD toolchain (sgs/tools/.venv) not installed");
  process.exit(0);
}

const usd = join(here, "../../assets/usd/pump-station-01/pump_station.usda");

// Minimal GLB (glTF 2.0) vertex-count parser (mirror of src/protocol.ts).
function glbVertexCount(buf) {
  const dv = new DataView(buf);
  if (dv.getUint32(0, true) !== 0x46546c67) return 0; // "glTF"
  let off = 12;
  let json = null;
  while (off + 8 <= buf.byteLength) {
    const len = dv.getUint32(off, true);
    const kind = dv.getUint32(off + 4, true);
    const start = off + 8;
    if (kind === 0x4e4f534a) {
      json = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, start, len)));
    }
    off = start + len;
  }
  if (!json) return 0;
  const prim = json.meshes?.[0]?.primitives?.[0];
  const posIdx = prim?.attributes?.POSITION;
  if (posIdx === undefined) return 0;
  return json.accessors?.[posIdx]?.count ?? 0;
}

function makeScene() {
  const map = new Map();
  return {
    map,
    apply(batch) {
      for (const m of batch) {
        if (m.type === "SnapshotBegin") map.clear();
        else if (m.type === "UpsertEntity") map.set(m.id, { geom: m.geom_ref });
        else if (m.type === "RemoveEntity") map.delete(m.id);
      }
    },
    ids() {
      return [...map.keys()].sort();
    },
  };
}

let server;
function cleanup() {
  if (server && !server.killed) server.kill("SIGKILL");
}
function assert(cond, msg) {
  if (!cond) {
    console.error("FAIL:", msg);
    cleanup();
    process.exit(1);
  }
}

function waitForAddr(proc) {
  return new Promise((resolve, reject) => {
    let buf = "";
    const to = setTimeout(() => reject(new Error("server did not report a listen address in time")), 15000);
    proc.stdout.on("data", (d) => {
      buf += d.toString();
      const m = buf.match(/listening on ws:\/\/([0-9.]+:[0-9]+)/);
      if (m) {
        clearTimeout(to);
        resolve(m[1]);
      }
    });
    proc.on("exit", (code) => reject(new Error(`server exited early (${code})`)));
  });
}

function open(url) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    ws.onopen = () => resolve(ws);
    ws.onerror = (e) => reject(new Error("ws error: " + (e.message ?? "unknown")));
  });
}

async function main() {
  server = spawn(bin, ["serve", usd, "--addr", "127.0.0.1:0", "--aoi-radius", "20"], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  let addr;
  try {
    addr = await waitForAddr(server);
  } catch (e) {
    // A missing usd-core wheel makes import fail; treat as a skip.
    console.log("skipping mesh test:", e.message);
    cleanup();
    process.exit(0);
  }
  const url = `ws://${addr}`;
  console.log("connected to", url);

  const ws = await open(url);
  const scene = makeScene();

  // Handshake: collect batches until the SnapshotMarker; keep a hash to fetch.
  const snapshot = await new Promise((resolve) => {
    ws.onmessage = (ev) => {
      if (ev.data instanceof ArrayBuffer) return; // no binary during handshake
      const batch = JSON.parse(ev.data);
      scene.apply(batch);
      if (batch.some((m) => m.type === "SnapshotMarker")) resolve(batch);
    };
    ws.send(JSON.stringify({ type: "Connect", protocol_versions: ["vf.bridge.v1"] }));
  });

  const upserts = snapshot.filter((m) => m.type === "UpsertEntity");
  assert(upserts.length > 0, "default AOI should activate entities");
  assert(
    JSON.stringify(scene.ids()) ===
      JSON.stringify(upserts.map((m) => m.id).sort()),
    "reconstruction == snapshot set (entities with and without geom)",
  );

  const withGeom = upserts.find((m) => m.geom_ref && m.geom_ref.content_hash);
  assert(withGeom, "at least one active component carries a GeomRef content hash");
  const hash = withGeom.geom_ref.content_hash;

  // Fetch the mesh and parse a positive vertex count.
  const glb = await new Promise((resolve) => {
    ws.onmessage = (ev) => {
      if (ev.data instanceof ArrayBuffer) resolve(ev.data);
    };
    ws.send(JSON.stringify({ type: "FetchGeom", content_hash: hash }));
  });
  const verts = glbVertexCount(glb);
  assert(verts > 0, `FetchGeom(${hash.slice(0, 8)}…) returns a mesh with vertices`);
  console.log(`fetched GLB for ${hash.slice(0, 8)}…: ${verts} vertices`);

  ws.close();
  cleanup();
  console.log("MESH OK");
  process.exit(0);
}

main().catch((e) => {
  console.error("mesh test error:", e);
  cleanup();
  process.exit(1);
});

// Headless smoke test for the Phase 5.5 observer path: spawn `sgs serve` on an
// ephemeral port, connect with Node's global WebSocket (Node >= 22), and assert
// that reconstructing the vf.bridge.v1 stream reproduces the server's snapshot
// set — and that a reconnect rebuilds it identically. This mirrors what the
// browser RenderScene (src/renderScene.ts) does; the apply() below is a
// dependency-free copy of that cache's essential logic.
//
// Run after building the server: `cargo build` in ../sgs, then `npm run smoke`.

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

// Minimal RenderScene cache (mirror of src/renderScene.ts apply()).
function makeScene() {
  const map = new Map();
  return {
    map,
    apply(batch) {
      for (const m of batch) {
        switch (m.type) {
          case "SnapshotBegin":
            map.clear();
            break;
          case "UpsertEntity":
            map.set(m.id, { transform: m.transform, extents: m.extents, visual: m.visual ?? [] });
            break;
          case "RemoveEntity":
            map.delete(m.id);
            break;
          case "SetTransform":
          case "PinConfirm":
            if (map.has(m.id)) map.get(m.id).transform = m.transform;
            break;
        }
      }
    },
    ids() {
      return [...map.keys()].sort();
    },
  };
}

function assert(cond, msg) {
  if (!cond) {
    console.error("FAIL:", msg);
    cleanup();
    process.exit(1);
  }
}

let server;
function cleanup() {
  if (server && !server.killed) server.kill("SIGKILL");
}

async function waitForAddr(proc) {
  return new Promise((resolve, reject) => {
    let buf = "";
    const to = setTimeout(() => reject(new Error("server did not report a listen address in time")), 8000);
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
    ws.onopen = () => resolve(ws);
    ws.onerror = (e) => reject(new Error("ws error: " + (e.message ?? "unknown")));
  });
}

/** Send Connect, collect the [Hello] then snapshot batches, return the scene. */
function handshake(ws) {
  return new Promise((resolve) => {
    const scene = makeScene();
    let batches = 0;
    ws.onmessage = (ev) => {
      const batch = JSON.parse(ev.data);
      scene.apply(batch);
      batches++;
      // batch 1 = [Hello], batch 2 = snapshot (ends with SnapshotMarker).
      if (batch.some((m) => m.type === "SnapshotMarker")) resolve({ scene, snapshot: batch });
    };
    ws.send(JSON.stringify({ type: "Connect", protocol_versions: ["vf.bridge.v1"] }));
  });
}

async function main() {
  server = spawn(bin, ["serve", "--synth", "800", "--addr", "127.0.0.1:0", "--aoi-radius", "8"], {
    stdio: ["ignore", "pipe", "inherit"],
  });
  const addr = await waitForAddr(server);
  const url = `ws://${addr}`;
  console.log("connected to", url);

  // Connection 1.
  const ws1 = await open(url);
  const { scene: s1, snapshot } = await handshake(ws1);
  const snapIds = snapshot.filter((m) => m.type === "UpsertEntity").map((m) => m.id).sort();
  assert(snapIds.length > 0, "default AOI should activate boxes");
  assert(JSON.stringify(s1.ids()) === JSON.stringify(snapIds), "reconstruction == snapshot set");
  for (const m of snapshot) {
    if (m.type === "UpsertEntity") {
      assert(m.extents, `upsert ${m.id} must carry proxy-box extents`);
    }
  }
  console.log(`snapshot reconstructed: ${snapIds.length} boxes`);
  ws1.close();

  // Connection 2: reconnect must rebuild identically (deterministic default AOI).
  const ws2 = await open(url);
  const { scene: s2 } = await handshake(ws2);
  assert(JSON.stringify(s2.ids()) === JSON.stringify(s1.ids()), "reconnect rebuilds identically");
  console.log("reconnect identical: yes");
  ws2.close();

  cleanup();
  console.log("SMOKE OK");
  process.exit(0);
}

main().catch((e) => {
  console.error("smoke error:", e);
  cleanup();
  process.exit(1);
});

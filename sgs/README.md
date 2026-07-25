# vectorflow-sgs — Scene Graph Service (Phases 1–5.6)

VectorFlow3D Scene Graph Service ([`../vectorflow3d-spec-scenegraph.md`](../vectorflow3d-spec-scenegraph.md)):

- **Phase 1** (§3.0–§3.1, §4.1, §4.5): index an OpenUSD root layer into an
  in-memory **Logical Scene Graph (LSG)** and persist runtime opinions
  (pins/overrides) in a SQLite **Twin Overlay**.
- **Phase 2** (§3.2–§3.3): **Interest / Subscription Manager** + **Runtime Scene
  Graph** — subscriptions (camera AOI + explicit selection) activate/deactivate
  entities over a coarse spatial index, payloads load/unload on demand, RSG
  pages are shared across subscribers, and unreferenced entities evict.
- **Phase 3** (§3.4, §4.4): **lazy telemetry resolvers** — a `Resolver` trait
  with a blocking **VictoriaMetrics** PromQL adapter that batches by metric
  (capped at `MAX_ASSETS_PER_QUERY`), a TTL / stale-while-revalidate cache living
  on the RSG, priority ordering, and an alert → forced-subscription seam.
- **Phase 4** (§3.9, §4.1–§4.2, §4.5): **Flow3D DSL** — a hand-written
  lexer/parser/compiler (`src/dsl/`) with line/column caret diagnostics that
  lowers `.flow3d` twin semantics (parts, tags, meta, anchors, pipes, bindings)
  into durable Twin-Overlay `Opinion`s (`src/opinion.rs`) and patches the LSG in
  place with **stable IDs** and **incremental reload** — no USD writes.
- **Phase 5** (§3.5, §5, §6.4): **Renderer Bridge API (`vf.bridge.v1`)** — a
  `serde` snapshot+diff message schema (`src/bridge.rs`) plus a `BridgeServer`
  that drains the RSG diff seam into `UpsertEntity`/`RemoveEntity`, resolves
  transforms pin > authored, forwards telemetry as visual state, writes pins
  back to the Twin Overlay, and answers coarse ray-AABB picks. A `FakeBridge`
  (`src/fake_bridge.rs`) reconstructs a disposable Render Scene cache from the
  message stream + fixture USD and resyncs identically on reconnect — all
  **in-process** (a real transport is Phase 6–7), with **zero USD writes**.
- **Phase 5.5** (§3.5 Transport amendment, §3.6, §5): **`sgs serve`** — the
  `vf.bridge.v1` stream over a **real WebSocket** for the `observer` WebGPU
  client. Blocking [`tungstenite`](https://crates.io/crates/tungstenite) over
  `std::net::TcpListener`, **thread-per-connection, no tokio** (matching the
  `ureq` ethos); one `Subscription` per connection; inbound `BridgeRequest`s map
  to the same `BridgeServer` methods and `BridgeMsg` batches stream down. The
  browser client ([`../web/`](../web/)) renders the active set as **AABB proxy
  boxes** tinted by telemetry quality and sends camera/pick/pin back. `extents`
  is an **additive** `Option<Aabb>` on `UpsertEntity` (still `vf.bridge.v1`);
  `EntityId` rides as a hex string so 64-bit ids survive `JSON.parse`. WebRTC /
  Wilbur (the SSR video path) are untouched.
- **Phase 5.6** (§3.1, §4.2, §3.6): **VF geometry store + WebGPU mesh
  hydration** — the content-addressed store that realizes the *"(future) VF
  geometry store"* named on `GeomRef`. `usd_export.py --mesh <asset> [prim]`
  tessellates a USD payload's gprims (deterministically, out-of-process) into
  points/normals/indices; [`src/geomstore.rs`](src/geomstore.rs) encodes each
  unique payload to **glTF 2.0 / GLB** (a small in-crate writer/reader, no glTF
  engine dependency) and caches it by `GeomRef.content_hash` (identical payloads
  tessellate **once**, mirroring the payload cache). The observer requests meshes
  with an additive `BridgeRequest::FetchGeom { content_hash }` and the server
  answers with the **GLB bytes as a binary WebSocket frame** over the same
  connection — no second port, no `v2` bump, still `vf.bridge.v1`. The browser
  hydrates + draws the real mesh; the Phase 5.5 **AABB proxy box remains the
  LOD-0 fallback** when a mesh is absent or in flight. The store is read-only and
  derived — **zero USD/LSG/overlay writes**; synthetic/NDJSON worlds have no
  on-disk mesh, so they keep the proxy box.

## What it does

```
pump_station.usda ──▶ usd_export.py (usd-core, Stage.Open LoadNone)
                          │  NDJSON, one record per prim
                          ▼
                    sgs import  ──▶  LSG (EntityId↔prim, extents, bindings, GeomRef)
                                      │
                    sgs pin/show ◀──▶ Twin Overlay (SQLite, pin > authored xform)
```

The USD import path composes the stage with **payloads unloaded**
(`Usd.Stage.Open(root, load=Usd.Stage.LoadNone)`), so component geometry is never
read — the LSG indexes only the instance-level data the interest layer needs
(transform, `extentsHint`, `customData.vf` identity/tags, `vf:binding:*`
descriptors) plus a `GeomRef` pointing at the still-closed payload.

**Domain boundary:** the importer, helper, interest, and hydration paths read
only USD files and the Twin Overlay — none of them touch VictoriaMetrics. Only
the **Phase 3 telemetry resolver** ([`src/resolver.rs`](src/resolver.rs)) speaks
PromQL, and only for entities already active in the RSG; resolved values live in
the RSG cache and are **never** written back to USD, the LSG, or the overlay.

## Build

```bash
cargo build --release        # from this directory
```

## USD helper setup (one-time)

The importer shells out to a small Python helper that uses OpenUSD via the
`usd-core` wheel. Create a venv and install it:

```bash
python3 -m venv sgs/tools/.venv
sgs/tools/.venv/bin/pip install -r sgs/tools/requirements.txt
```

Point the importer at that interpreter with `--python` (or `VF_USD_PYTHON`);
it defaults to `tools/.venv/bin/python3` then `python3`.

## Run

```bash
# Index the bootstrap fixture (runs the helper under the hood):
cargo run --release -- import ../assets/usd/pump-station-01/pump_station.usda

# Or ingest a pre-exported NDJSON dump (no Python needed):
cargo run --release -- import --from-json dump.ndjson

# Scale gate: build a synthetic ~10M-ref LSG and report time + RSS:
cargo run --release -- synth --count 10000000

# Twin Overlay pins (persist across restarts in the SQLite file):
cargo run --release -- pin PUMP-01 --translate 1,2,3 --by adam
cargo run --release -- show PUMP-01
cargo run --release -- unpin PUMP-01

# Phase 2: moving-AOI + selection interest demo over the RSG
cargo run --release -- interest ../assets/usd/pump-station-01/pump_station.usda \
  --aoi-center 0,3,0 --aoi-radius 5 --steps 4 --step-delta 5,0,0 --select SWG-01

# Phase 3: lazy telemetry resolution for the active RSG working set.
# --offline uses an in-process stub (no VM needed); drop it to hit a real VM.
cargo run --release -- resolve ../assets/usd/pump-station-01/pump_station.usda \
  --offline --aoi-center 0,3,0 --aoi-radius 8 --steps 3 --alert SWG-01
# Against a running VictoriaMetrics (see ../infra/victoriametrics):
cargo run --release -- resolve ../assets/usd/pump-station-01/pump_station.usda \
  --vm-url http://127.0.0.1:8428 --aoi-center 0,3,0 --aoi-radius 8

# Phase 4: compile a Flow3D DSL file into Twin-Overlay opinions + LSG patch.
cargo run --release -- compile ../assets/flow3d/pump-station-01.flow3d \
  ../assets/usd/pump-station-01/pump_station.usda
# ...with an incremental-reload demo (proves no RSG storm on re-compile):
cargo run --release -- compile ../assets/flow3d/pump-station-01.flow3d \
  ../assets/usd/pump-station-01/pump_station.usda \
  --reload ../assets/flow3d/pump-station-01.reload.flow3d

# Phase 5: drive a fake renderer bridge over vf.bridge.v1 (snapshot + diff),
# with a pin write-back and a disconnect/reconnect resync (no engine needed):
cargo run --release -- bridge ../assets/usd/pump-station-01/pump_station.usda \
  --aoi-center 0,3,0 --aoi-radius 8 --select SWG-01 --pin PUMP-01 --pin-translate 0,0,5
# Python-free synthetic variant (stub geometry hydration):
cargo run --release -- bridge --synth 5000 --aoi-radius 8 --steps 4 --pin /Synth/e0

# Phase 5.5: serve vf.bridge.v1 over a WebSocket for the browser WebGPU client
# (blocking, no tokio). Offline stub telemetry by default (no VM needed):
cargo run --release -- serve ../assets/usd/pump-station-01/pump_station.usda \
  --addr 127.0.0.1:8787 --aoi-radius 10
# Python-free synthetic world, or live VictoriaMetrics tints:
cargo run --release -- serve --synth 20000 --addr 127.0.0.1:8787
cargo run --release -- serve --synth 20000 --vm --vm-url http://127.0.0.1:8428
```

Then start the browser observer (separate terminal):

```bash
cd ../web
npm install
npm run dev            # open the printed http://127.0.0.1:5173 in a WebGPU browser
# point it elsewhere with ?ws=ws://host:port or the HUD field
npm run smoke          # headless: spawns `sgs serve --synth`, asserts reconstruction == snapshot
npm run smoke:mesh     # headless: spawns `sgs serve <pump-station usd>`, fetches a GLB by hash (Phase 5.6)
```

Serving a real USD root (not `--synth`/`--from-json`) makes the observer fetch
GLB meshes from the VF geometry store by `content_hash` and render real
pump/tank/switch geometry; boxes remain the LOD-0 fallback while a mesh is absent
or in flight.

Controls: drag = orbit, scroll = zoom (grows the AOI), WASD / arrows or
shift-drag = move the AOI focus (the active set streams in/out), click = coarse
pick, **P** = pin the selected box (persists to the Twin Overlay; the box snaps
to the `PinConfirm`), **U** = unpin. Reloading the page reconnects and rebuilds
the scene identically from a fresh snapshot. Boxes are tinted by telemetry
quality: green `ok`, amber `stale`, red `unavailable` (use `--outage` to force
stale/unavailable tints).

`compile` prints the opinion diff (`+added / ~changed / -removed / unchanged`),
touched-entity count, anchors/edges, and the LSG + overlay revisions. Syntax and
semantic errors are reported with a `file:line:col` locator and a caret, and the
compiler collects every error in one pass rather than stopping at the first. The
`--reload` demo holds a camera AOI's active set, applies the edited DSL as a
minimal patch, and re-evaluates interest to show zero activate/deactivate churn.

Each `resolve` step prints `|RSG|`, bindings issued vs cache hits (hit ratio),
`ok`/`stale`/`unavailable` counts, and the upstream (VM) round-trips added —
metric batching keeps those far below the binding count (e.g. the fixture's 29
bindings resolve in 12 queries). `--outage` simulates a down resolver to show
stale-while-revalidate. The overlay database defaults to
`./vf-twin-overlay.sqlite` (override with `--overlay <path>` or `VF_OVERLAY_DB`).

## Layout

```
sgs/
├── Cargo.toml
├── src/
│   ├── main.rs        # CLI (import / synth / pin / unpin / show / interest / resolve / compile / bridge / serve)
│   ├── lsg.rs         # LSG types + in-memory index + stable EntityId + DSL patch mutators
│   ├── import.rs      # invoke helper / read NDJSON -> build LSG
│   ├── overlay.rs     # SQLite Twin Overlay: pins + DSL opinions + compile stamps
│   ├── opinion.rs     # Twin-Overlay opinions: stable keys, hashes, diff, apply/reconcile (Phase 4)
│   ├── synth.rs       # synthetic ~10M-ref generator (scale gate)
│   ├── spatial.rs     # coarse uniform-grid broad-phase index (Phase 2)
│   ├── interest.rs    # Interest / Subscription Manager (Phase 2)
│   ├── rsg.rs         # Runtime Scene Graph: shared pages, eviction, telemetry
│   ├── hydrate.rs     # payload load/unload cache (Phase 2)
│   ├── resolver.rs    # lazy telemetry resolvers: VM PromQL + TTL cache (Phase 3)
│   ├── alert.rs       # alert -> forced-subscription seam (Phase 3)
│   ├── dsl/           # Flow3D DSL (Phase 4): lexer, parser, ast, diag, compile
│   ├── bridge.rs      # Renderer Bridge API (vf.bridge.v1): schema + BridgeServer (Phase 5)
│   ├── fake_bridge.rs # in-process fake renderer: Render Scene cache from diffs (Phase 5)
│   ├── geomstore.rs   # content-addressed VF geometry store: USD->GLB, FetchGeom (Phase 5.6)
│   └── serve.rs       # blocking WebSocket server for the observer WebGPU client (Phase 5.5/5.6)
├── tools/
│   ├── usd_export.py  # OpenUSD (usd-core): index->NDJSON / --payload / --mesh (GLB tessellation)
│   └── requirements.txt
└── tests/             # phase1.rs .. phase5.rs / phase5_5.rs / phase5_6.rs integration tests
```

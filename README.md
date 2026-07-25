# VectorFlow3D

Open-source, renderer-agnostic digital twin runtime: massive OpenUSD worlds, selective high-fidelity viewers, live telemetry via VictoriaMetrics, and an O3DE client.

Architecture: [vectorflow3d-spec-scenegraph.md](vectorflow3d-spec-scenegraph.md)

## Scene Graph Service (SGS) — Rust runtime

The `[sgs/](sgs/)` crate is the runtime state engine. 

**Phase 1** indexes an OpenUSD root layer into the Logical Scene Graph (payloads unloaded) and keeps committed pins in a SQLite Twin Overlay. **Phase 2** adds the Interest / Subscription Manager and the Runtime Scene Graph: subscriptions (moving camera AOI + explicit selection) activate/deactivate entities over a coarse spatial index, USD payloads load/unload on demand (surfacing the component-internal defaults the index pass could not see), RSG pages are shared across subscribers, and unreferenced entities evict after a grace period. 

**Phase 3** adds lazy telemetry resolvers: a `Resolver` trait with a blocking **VictoriaMetrics** PromQL adapter that batches by metric (`metric{asset=~"A|B|C"}`) and demuxes by the `asset` label, a TTL / stale-while-revalidate cache living on the RSG, high-priority-first ordering, and an alert → forced-subscription seam — all with **zero USD writes**. **Phase 4** adds the **Flow3D DSL**: a hand-written `.flow3d` lexer/parser/compiler with line/column caret diagnostics that lowers twin semantics (tags, metadata, anchors, pipes, bindings) to durable Twin-Overlay opinions and patches the LSG in place with stable IDs and incremental reload — no USD writes. **Phase 5** adds the **Renderer Bridge API (`vf.bridge.v1`)**: a `serde` snapshot+diff message schema + a `BridgeServer` that drains the RSG diff seam, resolves transforms pin > authored, writes pins back to the Twin Overlay, and answers coarse ray-AABB picks; an in-process `FakeBridge` reconstructs a disposable Render Scene from the message stream + fixture USD and resyncs identically on reconnect (no engine required; a real transport is Phase 6–7). 

**Phase 5.5** puts that bridge on a **real wire** for the `observer` profile: `**sgs serve`** streams `vf.bridge.v1` over a blocking WebSocket (sync `tungstenite`, **no tokio**, thread-per-connection) and a browser **WebGPU** client (`[web/](web/)`) reconstructs the active set as **AABB proxy boxes** tinted by telemetry quality, sending camera/pick/pin back. `extents` rides the wire as an additive field (still `vf.bridge.v1`); WebRTC/Wilbur (the SSR video path) stay untouched. 

**Phase 5.6** turns those proxy boxes into **real geometry**: a content-addressed **VF geometry store** tessellates USD payloads out-of-process into **glTF 2.0 / GLB** (keyed by `GeomRef.content_hash`, tessellated once), the observer fetches meshes with an additive `FetchGeom` → **binary WebSocket frame** on the same connection, and the WebGPU client draws real pump/tank/switch meshes — keeping the AABB proxy box as the **LOD-0 fallback** when a mesh is absent or in flight. Still `vf.bridge.v1`, still zero USD/LSG writes.

```bash
cd sgs
cargo build --release
cargo test                                          # 56 unit + 6 P1 + 7 P2 + 6 P3 + 6 P4 + 8 P5 + 3 P5.5 + 2 P5.6 = 94

# Phase 1: index the sample facility (payloads stay unloaded)
./target/release/sgs import ../assets/usd/pump-station-01/pump_station.usda

# Phase 2: drive a moving-AOI + selection interest demo over the RSG
./target/release/sgs interest ../assets/usd/pump-station-01/pump_station.usda \
  --aoi-center 0,3,0 --aoi-radius 5 --steps 4 --step-delta 5,0,0 \
  --grace-steps 1 --select SWG-01

# Phase 3: resolve telemetry lazily for the active RSG (--offline needs no VM)
./target/release/sgs resolve ../assets/usd/pump-station-01/pump_station.usda \
  --offline --aoi-center 0,3,0 --aoi-radius 8 --steps 3 --alert SWG-01

# Phase 4: compile a Flow3D DSL file into Twin-Overlay opinions + LSG patch
./target/release/sgs compile ../assets/flow3d/pump-station-01.flow3d \
  ../assets/usd/pump-station-01/pump_station.usda \
  --reload ../assets/flow3d/pump-station-01.reload.flow3d

# Phase 5: drive a fake renderer bridge over vf.bridge.v1 (pin write-back + reconnect resync)
./target/release/sgs bridge ../assets/usd/pump-station-01/pump_station.usda \
  --aoi-center 0,3,0 --aoi-radius 8 --select SWG-01 --pin PUMP-01 --pin-translate 0,0,5

# Phase 5.5: serve vf.bridge.v1 over a WebSocket, then open the browser WebGPU client
./target/release/sgs serve ../assets/usd/pump-station-01/pump_station.usda --addr 127.0.0.1:8787
# separate terminal:
cd ../web && npm install && npm run dev   # open http://127.0.0.1:5173 in a WebGPU browser
```

The `serve` path is the **observer** profile "rendering today": drag to orbit, scroll to zoom, pan (WASD / shift-drag) to move the area of interest — geometry streams in/out as the AOI moves, click coarse-picks an entity, **P** pins the selection (persisted to the Twin Overlay; it snaps to the `PinConfirm`), and reloading reconnects and rebuilds the scene identically. When served from a real USD root, the observer fetches **GLB meshes** by `content_hash` from the VF geometry store and renders real pump/tank/switch geometry (Phase 5.6), falling back to proxy boxes while a mesh is absent or in flight; entities are tinted green `ok` / amber `stale` / red `unavailable` from the telemetry the resolver forwards.

The `interest` demo prints eval time, activate/deactivate deltas, `|RSG|`, open-payload count, evictions, and the per-subscriber diff. The `resolve` demo prints `|RSG|`, bindings issued vs cache hits, `ok`/`stale`/`unavailable` counts, and upstream VM round-trips (metric batching keeps these far below the binding count — the fixture's 29 bindings resolve in 12 queries). Use `--synth <N>` instead of a USD path for a synthetic world (e.g. `--synth 10000000`): interest eval stays **~0.3–1.0 ms/step** with `|RSG|`, open payloads, and upstream queries bounded regardless of world size — massive world, selective activation and resolution.

**Domain boundary:** the SGS's import, interest, and hydration paths read only USD files and the Twin Overlay and never contact VictoriaMetrics. Only the **telemetry resolver** speaks PromQL, and only for entities already active in the RSG; resolved values live in the RSG cache and are never written back to USD. **O3DE never queries VictoriaMetrics.**

## Quick start — metrics (Docker)

```bash
docker compose -f infra/victoriametrics/docker-compose.yml up -d
# or: docker compose -f infra/docker-compose.yml up -d
```

- UI: [http://localhost:8428/vmui](http://localhost:8428/vmui)  
- PromQL: [http://localhost:8428/api/v1/query?query=up](http://localhost:8428/api/v1/query?query=up)

### Seed sample telemetry

Populate VictoriaMetrics with synthetic series for the assets in [assets/usd/pump-station-01/](assets/usd/pump-station-01/) — using the exact metric names and `asset` labels the scene's `vf:binding:*` queries expect (e.g. `pump_flow_gpm{asset="PUMP-01"}`). Requires the metrics stack (above) running.

```bash
./scripts/seed-victoriametrics.sh                 # one-shot: last 60 min @ 30s, ending now
./scripts/seed-victoriametrics.sh --live          # backfill, then append one sample/series every step
./scripts/seed-victoriametrics.sh --pumps 500 --switches 500 --window-min 10   # throughput sanity check
```


| Flag                                       | Default                 | Meaning                                                   |
| ------------------------------------------ | ----------------------- | --------------------------------------------------------- |
| `--window-min N`                           | `60`                    | History window (minutes) ending now                       |
| `--step SECONDS`                           | `30`                    | Sample interval → `~window*60/step` points/series         |
| `--pumps N` / `--tanks N` / `--switches N` | `3` / `2` / `2`         | Asset counts (defaults reproduce the scene exactly)       |
| `--live`                                   | off                     | After backfill, keep appending fresh samples until Ctrl-C |
| `--url URL` (or `VF_VM_URL`)               | `http://127.0.0.1:8428` | VictoriaMetrics base URL                                  |


It writes via VM's Prometheus text import (`/api/v1/import/prometheus`) and prints a samples/sec throughput summary. Verify:

```bash
curl 'http://localhost:8428/api/v1/query?query=pump_flow_gpm{asset="PUMP-01"}'
# or open vmui and plot: tank_level_pct  /  switch_load_amps
```

**Staleness:** instant PromQL only returns a value if a sample exists within VM's lookback (~5 min). The one-shot seed ends at "now"; re-run it to refresh, or use `--live` for a continuously-fresh dev session.

**Historical CSV / IoT import (spec's route):** for real historical backfill, land data in VM rather than teaching the resolver to parse CSV. VM ingests CSV directly:

```bash
# columns: unix_seconds, asset label, value  ->  metric pump_flow_gpm
curl --data-binary @flow.csv \
  'http://localhost:8428/api/v1/import/csv?format=1:time:unix_s,2:label:asset,3:metric:pump_flow_gpm'
```

See VM's [CSV import docs](https://docs.victoriametrics.com/victoriametrics/single-server-victoriametrics/#how-to-import-data-in-csv) (also linked in [infra/victoriametrics/docker-compose.yml](infra/victoriametrics/docker-compose.yml)).

**Domain boundary:** this is the Telemetry Resolver's PromQL surface. **O3DE never queries VictoriaMetrics.**

### Local health check

```bash
./scripts/healthcheck-local.sh                     # VM healthy + O3DE built (or Docker client)
./scripts/healthcheck-local.sh --strict            # also require O3DE process/container running
./scripts/healthcheck-local.sh --pixelstreaming    # also check Pixel Streaming (opt-in)
```

The **Telemetry Resolver** (VectorFlow / SGS) queries VictoriaMetrics. **O3DE never sees PromQL or VM credentials.**

## Quick start — Pixel Streaming (renderer output test)

Stand up Epic's Pixel Streaming Infrastructure (Wilbur signalling, formerly Cirrus) plus a **reference streamer** that pushes synthetic test-pattern video over the same WebRTC path the future O3DE Streamer will use. This is opt-in and separate from the metrics stack.

```bash
docker compose -f infra/pixelstreaming/docker-compose.yml \
  --profile pixelstreaming --profile streamer up -d --build
```

Open [http://localhost/](http://localhost/) and play — the color-bar test pattern (with live timestamp + frame `seq`) **is** the renderer output. Details, the `FrameSource` seam O3DE replaces later, the SFU host-networking note (Linux-only) + macOS fallback, and the scope limitation of synthetic-frame testing: [infra/pixelstreaming/README.md](infra/pixelstreaming/README.md). Spec context: [§3.7 / Phase 7](vectorflow3d-spec-scenegraph.md).

## O3DE as a local client (valid use case)

Installing and running O3DE on your machine is a **first-class client**: local GPU render + future VectorFlow Gem over `vf.bridge.v1` to the Scene Graph Service. No browser video stream required for this path.


| Path                    | Host                                | When to use                                                          |
| ----------------------- | ----------------------------------- | -------------------------------------------------------------------- |
| **Native client**       | macOS, Linux, Windows               | Preferred local client — full local GPU render                       |
| **Docker Linux client** | Any Docker host (GPU on Linux only) | Portable client role; **current active path on this Mac** (see note) |
| **SSR worker**          | Linux or Windows GPU                | Stream to browser operators (later phases)                           |


Docker Desktop on **macOS cannot** GPU-accelerate a Linux O3DE container, and native Apple-Silicon O3DE is currently **blocked upstream** (missing `DirectXShaderCompilerDxc-*-mac-arm64` package on the O3DE CDN). So on this Mac we run the **Docker Linux `o3de-client`** to fill the local-client role today (a stub until the VectorFlow Gem ships — no local render yet; we optimize render later). See [O3DE in a Linux Docker container](#optional-o3de-in-a-linux-docker-container-local-client).

Env contract: [infra/o3de/env.example](infra/o3de/env.example)

### Domain boundary (locked)

```
VictoriaMetrics  --PromQL-->  Telemetry Resolver (SGS)
                                    |
                              visual / transform diffs
                                    |
                              vf.bridge.v1
                                    |
                                   O3DE
```

O3DE only consumes the Renderer Bridge API. Telemetry systems and rendering engines stay separate.

---

## Install O3DE on macOS (local client)

Upstream macOS support is **experimental** (no official installer — build from source). For this project it is still a **valid local client** path.

### Prerequisites

- macOS with **Xcode** (12.1+; this project verified against recent Xcode)
- **CMake** ≥ 3.30 (Homebrew: `brew install cmake`)
- **Git** + **Git LFS** (`brew install git-lfs && git lfs install`, or a release binary in `~/.local/bin`)
- **~100+ GB** free disk for a source build
- Metal-capable GPU

Check:

```bash
xcodebuild -version
cmake --version
git lfs version
```

Or run the helper (installs Git LFS via Homebrew if missing, then clones/configures):

```bash
./scripts/setup-o3de-mac.sh
```

### Clone and build

Default engine location: `~/O3DE/o3de` (outside this repo so Git LFS assets do not bloat the project).

```bash
mkdir -p ~/O3DE
cd ~/O3DE
git clone https://github.com/o3de/o3de.git
cd o3de
git lfs install
git lfs pull

# Engine Python
./python/get_python.sh

# Configure (Xcode generator)
cmake -B build/mac_xcode -S . -G Xcode \
  -DLY_ASSET_DEPLOY_MODE=LOOSE \
  -DLY_ASSET_DEPLOY_ASSET_TYPE=mac

# Build Editor (profile) — long first build
cmake --build build/mac_xcode --target Editor --config profile
```

Run:

```bash
open build/mac_xcode/bin/profile/Editor.app
# or:
./build/mac_xcode/bin/profile/Editor.app/Contents/MacOS/Editor
```

**Known blocker on Apple Silicon (as of this writing):** the O3DE CMake configure fails because `DirectXShaderCompilerDxc-*-mac-arm64` is **not published** on the O3DE CDN for **any** rev — the arm64 manifest entries exist but the binary was never uploaded (the `-mac` Intel package *is* present). This affects `development` **and** stable tags (e.g. `2605.0` references the same missing package), so pinning to a release does not help. Track [O3DE Discord `#sig-platform](https://discord.com/invite/o3de)` and [supported platforms](https://www.docs.o3de.org/docs/welcome-guide/supported-platforms/); the clone at `~/O3DE/o3de` can be reused once upstream publishes the arm64 DXC package.

> A separate, unrelated configure error (`tinyusdz_repo ... patch does not apply`) is just a stale, already-patched `_deps` tree from a prior failed run — clean it with `git -C ~/O3DE/o3de/build/mac_xcode/_deps/tinyusdz_repo-src checkout -- .` (or delete the `_deps/assimp-`* and `_deps/tinyusdz_repo-`* dirs) and re-configure.

**Until upstream ships arm64 DXC, this Mac uses the [Docker Linux `o3de-client](#optional-o3de-in-a-linux-docker-container-local-client)*`* to fill the local-client role.

**This machine (setup status):** Git LFS installed to `~/.local/bin`; engine cloned to `~/O3DE/o3de`; native configure blocked on the upstream arm64 DXC package above; **Docker `o3de-client` (stub) running** as the current local client.

Official requirements: [System requirements](https://www.docs.o3de.org/docs/welcome-guide/requirements/)

---

## Install O3DE on Linux or Windows

Use official installers when possible (first-class hosts):

- [Linux install](https://www.docs.o3de.org/docs/welcome-guide/setup/installing-linux/)
- [Windows install](https://www.docs.o3de.org/docs/welcome-guide/setup/installing-windows/)
- Prerequisites: [Requirements](https://www.docs.o3de.org/docs/welcome-guide/requirements/)

Same roles on both OS families:

- **Local client** — desktop Editor/launcher + VectorFlow Gem → SGS  
- **SSR worker** — GPU host for streamed browser sessions (Orchestrator / Streamer come later)

NVIDIA drivers + Vulkan (Linux) or DirectX 12 / Vulkan (Windows) recommended for serious rendering / later encode.

---

## Optional: O3DE in a Linux Docker container (local client)

The base compose is **portable** and runs anywhere Docker runs — including macOS / Apple Silicon (built and run natively for `linux/arm64`, no GPU). This is the **current active local-client path on this Mac** while native Apple-Silicon O3DE stays blocked upstream.

```bash
# Portable (macOS / no-GPU) — current working path:
docker compose -f infra/o3de/docker-compose.yml --profile o3de-client up -d --build
```

For **Linux hosts with NVIDIA Container Toolkit**, layer on the GPU override for real passthrough:

```bash
docker compose -f infra/o3de/docker-compose.yml -f infra/o3de/docker-compose.gpu.yml \
  --profile o3de-client up -d --build
```

Verify:

```bash
docker logs vf-o3de-client        # prints the bridge stub banner
./scripts/healthcheck-local.sh    # should report: OK Docker O3DE client (vf-o3de-client) running
```

Stop / remove:

```bash
docker compose -f infra/o3de/docker-compose.yml --profile o3de-client down
```

> **What this is today:** a **stub** entrypoint (prints `vf.bridge.v1` env, then idles) that satisfies the `o3de-client` role for the health check and domain-boundary wiring. It does **not** render yet and **does not** replace a native install — the real Editor/GameLauncher + VectorFlow Gem image lands in **spec Phase 6**. It never receives PromQL / VictoriaMetrics URLs (domain boundary). See comments in [infra/o3de/docker-compose.yml](infra/o3de/docker-compose.yml).

---

## Repo layout

```
sgs/                     # Scene Graph Service (Rust): LSG index + Twin Overlay (P1), Interest Manager + RSG (P2), lazy telemetry resolvers (P3), Flow3D DSL (P4), Renderer Bridge vf.bridge.v1 + fake bridge (P5)
assets/usd/              # OpenUSD bootstrap scenes (import-time scene description; see assets/usd/pump-station-01/)
infra/victoriametrics/   # VictoriaMetrics (dev)
infra/o3de/              # O3DE env + optional Linux Docker client
infra/pixelstreaming/    # Pixel Streaming (Wilbur + SFU) + reference streamer/harness
scripts/setup-o3de-mac.sh
vectorflow3d-spec-scenegraph.md
```


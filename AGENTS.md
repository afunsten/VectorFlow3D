# VectorFlow3D — Agent Checklist

Brief status for agents working on this repo. Architecture and phase detail:
[`vectorflow3d-spec-scenegraph.md`](vectorflow3d-spec-scenegraph.md) (§7 Build
Phases). Runtime code: [`sgs/`](sgs/). This file is a quick orientation, not the
source of truth — the spec is.

## Build phases

| Phase | Status | What / key artifacts |
|---|---|---|
| 0 — Design locks | ✅ done | Scale motto, `viewer_profiles`, VF USD authoring convention (`vf:binding:*` + `customData.vf`), VictoriaMetrics-first. Spec §7 Phase 0. |
| 1 — OpenUSD index + Twin Overlay | ✅ done | LSG index (payloads unloaded) via out-of-process `usd-core` helper → NDJSON; SQLite Twin Overlay (pins). [`sgs/src/lsg.rs`](sgs/src/lsg.rs), [`import.rs`](sgs/src/import.rs), [`overlay.rs`](sgs/src/overlay.rs), [`tools/usd_export.py`](sgs/tools/usd_export.py). |
| 2 — Interest Manager + RSG | ✅ done | Subscriptions (AOI + selection), coarse spatial grid, shared RSG pages, grace eviction, payload hydrate cache, per-subscriber diffs. [`interest.rs`](sgs/src/interest.rs), [`rsg.rs`](sgs/src/rsg.rs), [`spatial.rs`](sgs/src/spatial.rs), [`hydrate.rs`](sgs/src/hydrate.rs). |
| 3 — Telemetry resolvers (lazy) | ✅ done | `Resolver` trait + VictoriaMetrics PromQL adapter, batch-by-metric (capped at `MAX_ASSETS_PER_QUERY`), RSG-resident TTL / stale-while-revalidate cache, priority, alert→forced-subscription seam. [`resolver.rs`](sgs/src/resolver.rs), [`alert.rs`](sgs/src/alert.rs). |
| 4 — Flow3D DSL ↔ USD/overlay | ✅ done | Hand-written `.flow3d` lexer/parser/compiler with line/col caret diagnostics; lowers to durable Twin-Overlay opinions; stable-ID, incremental in-place LSG patch; **no USD writes**. [`sgs/src/dsl/`](sgs/src/dsl/), [`opinion.rs`](sgs/src/opinion.rs), [`assets/flow3d/`](assets/flow3d/). |
| 5 — Renderer Bridge API + fake bridge | ✅ done | `vf.bridge.v1` `serde` snapshot+diff schema + `BridgeServer` (reads LSG/RSG/overlay, owns none); pin/unpin write-back to the Twin Overlay; coarse ray-AABB pick; `FakeBridge` reconstructs a disposable Render Scene from diffs + fixture USD and resyncs identically on reconnect — in-process (transport deferred to Ph6-7), zero USD writes. [`bridge.rs`](sgs/src/bridge.rs), [`fake_bridge.rs`](sgs/src/fake_bridge.rs). |
| 5.5 — WebGPU Observer Client (WebSocket) | ✅ done | Realizes Phase 5 over a **real wire** for `observer`: blocking `sgs serve` (sync `tungstenite`, **no tokio**, thread-per-connection, one Subscription/conn) + a browser WebGPU client rendering the active set as **AABB proxy boxes** tinted by telemetry quality, sending camera/pick/pin back. `extents` is an **additive** `Option<Aabb>` on `UpsertEntity` (still `vf.bridge.v1`); `EntityId` rides as a hex string (JS-safe). [`serve.rs`](sgs/src/serve.rs), [`web/`](web/). WebRTC/Wilbur untouched. |
| 5.6 — VF Geometry Store + WebGPU mesh hydration (glTF/GLB) | ✅ done | Content-addressed VF geometry store: `usd_export.py --mesh` tessellates USD payloads out-of-process; [`geomstore.rs`](sgs/src/geomstore.rs) encodes each unique payload to **GLB** once (small in-crate writer/reader, no glTF engine dep) and serves bytes over an additive `FetchGeom` → **binary WS frame** (same connection, still `vf.bridge.v1`). Observer hydrates + draws real pump/tank/switch meshes; the **AABB proxy box stays LOD-0** when a mesh is absent/in-flight. Zero USD/LSG writes. [`serve.rs`](sgs/src/serve.rs), [`web/`](web/). |
| 6 — O3DE Bridge (Render Scene cache) | ⛔ todo | O3DE Gem applies bridge diffs, hydrates geom (reuses the Phase 5.6 store), exposes render target as a `FrameSource`. |
| 7 — Streaming (Wilbur + O3DE Streamer) | ⛔ todo (plumbing scaffolded) | Wilbur+SFU + reference streamer/CI harness exist ([`infra/pixelstreaming/`](infra/pixelstreaming/)); the real O3DE-sourced encode behind the `FrameSource` seam is **not built or measured** — see Known open risks. |
| 8 — Interaction polish | ⛔ todo | Pick path, pin precedence (enforce §4.1 total order), overlay hints, client drag prediction. |
| 9 — Orchestrator + multi-session | ⛔ todo | Worker pool for ~100 operators, sticky routing, crash-replace, authn/authz + profile enforcement. |
| 10 — Hardening | ⛔ todo | Metrics/tracing, load suite vs §6.5 gates, optional 2nd resolver / Unreal / WebGPU spike, optional USD override-layer export. |

## Known open risks (watch)

- **Phase 7 real encode is unmeasured (spec §8 risk #2).** The synthetic-frame
  harness validates signalling/transport/protocol **plumbing only**. A green
  harness is **not** evidence that real O3DE-sourced capture/encode/WebRTC works
  under real CAD content — that is a separate, still-open gate. Do not treat
  streaming as de-risked until the O3DE `FrameSource` is swapped in and
  glass-to-glass is measured against real geometry.
- **SQLite Twin Overlay is single-node** — fine until SGS must shard / multi-writer HA is a *measured* need (spec §8.8). Don't pre-shard.
- **Pin precedence** enforcement lands in Phase 8; the total order is locked in spec §4.1 (authored default < telemetry override < **pin (pending or committed)** — pending drag pins and committed pins share one rank).
- **Resolver is synchronous (`ureq`)** — the concrete trigger to revisit sync→async is *routine* `MAX_ASSETS_PER_QUERY` chunking under real automation-rule load (`N` chunks = `N×` latency per pass), not a speculative future need (spec §3.4 / Phase 3 caveat).

## Invariants (do not break)

- **Zero hot-path USD writes.** USD is import-only; runtime state lives in the
  RSG, durable opinions in the Twin Overlay. Verified in tests.
- **Only the resolver speaks PromQL** ([`resolver.rs`](sgs/src/resolver.rs));
  the LSG/import/interest/hydrate/DSL paths never touch VictoriaMetrics.
- **Bindings are declarative** — never store live telemetry values in USD, the
  LSG, or the Twin Overlay.

## Test / build

```bash
cd sgs && cargo test        # 94 tests across the crate (unit + phase1..5, 5.5, 5.6)
cargo build --release
```

USD import needs the one-time Python venv (`sgs/tools/`); NDJSON `--from-json`
and `--synth` paths are Python-free.

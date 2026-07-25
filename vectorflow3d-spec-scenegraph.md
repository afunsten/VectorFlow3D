# VectorFlow3D — Scene-Graph-Centric Architecture (v4.7)

## Technical Design Document

> **Lineage:** v1 streamed geometry to a custom browser renderer. v2 moved to server-side rendering + video streaming but coupled scene state to the render process. v3 introduced a renderer-agnostic Scene Graph Service. **v4 corrects the core modeling mistake in v3:** the Scene Graph Service is the authoritative *logical* world model — not a live mirror of telemetry — and introduces an explicit Subscription / Interest Management layer. **v4.1** locks OpenUSD as geometry/scene-description (not runtime). **v4.2** locks industrial viewer tiers. **v4.3** reframes scale as **massive world, selective high-fidelity viewers** — world size and render sessions scale independently. **v4.4** locks **VictoriaMetrics** as the default metrics backend for resolvers (PromQL-compatible; no Prometheus server required in front). **v4.5** locks OpenUSD as an **initialization / import format** that seeds the VectorFlow Scene Graph — **not a runtime dependency**; once imported, the Scene Graph owns geometry, materials, hierarchy, and metadata **defaults** and may scale/transform them from telemetry (and other inputs), renderer-independently. **v4.6** locks the **streaming plumbing**: reuse Epic **Wilbur** (the signalling server formerly named *Cirrus*) **+ SFU** via their official container images, and prove the WebRTC signalling/transport path with a permanent **reference streamer** (synthetic frames) that sits behind an explicit **FrameSource** seam the O3DE Streamer later replaces — synthetic-frame CI validates **plumbing only** and never substitutes for real O3DE-sourced encode / end-to-end validation. **v4.7** locks the concrete **pre-v1 VF USD authoring convention**: telemetry bindings as `vf:binding:<attribute>:*` namespaced attributes (`sourceId` / `query` / `attribute` / `unit` / `ttlMs` / `priority` / `qualityPolicy`) and identity/tags/static metadata in `customData.vf`; a **facility assembly → area group → component** hierarchy with components deferred behind `**payload`** arcs (Z-up, `metersPerUnit = 1`); the PromQL `asset` label carries the prim's `vf.assetTag`; reference sample at `assets/usd/pump-station-01/`, and a future codeless `VfTelemetryBindingAPI` will formalize the convention.

> **USD stance:** OpenUSD is the open, composable **initialization / import format** used to seed the VectorFlow Scene Graph with hierarchy, geometry, materials, payloads/LOD, authored metadata, and declarative telemetry *bindings* (via a VectorFlow schema). It is **not a runtime dependency**: the live runtime never requires an open, composed USD stage, and USD is **not** where live metric values, interest sets, per-viewer cameras, or interactive drag state live. Once imported, the Scene Graph owns geometry, materials, hierarchy, and metadata as **defaults** and may **scale/transform** them from telemetry (and other inputs) in a **renderer-independent** way. This is the opposite of the “write every tick into the stage” Omniverse-style anti-pattern.

> **Renderer stance:** O3DE remains the default **operator SSR** backend (Apache 2.0 / MIT). Unreal remains optional when Pixel Streaming maturity outweighs licensing cost. WebGPU / video / dashboards serve observers at scale behind the same Renderer Bridge API / query APIs. The renderer is always a **cache of the logical world**, never the source of truth.

> **Scale motto (v4.3):** Do **not** optimize VectorFlow3D to render a billion users. Optimize it so a **billion objects can exist** while only the users who need full simulation consume GPU resources. Same pattern as large-scale games, GIS, and industrial platforms: **massive world, selective high-fidelity viewers.**

---

## 0. Glossary


| Term                                                                  | Meaning                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **OpenUSD Stage**                                                     | Composed scene-description + geometry consumed to **initialize / import** the Scene Graph: prim hierarchy, references/payloads, variants, materials, authored transforms, static metadata, and VF declarative schemas. **Initialization / interchange format — not a runtime dependency.** Read at import/reindex time to seed defaults; the live runtime does not require an open stage.                                                                                  |
| **Logical Scene Graph (LSG)**                                         | SGS’s authoritative index **initialized from** the OpenUSD stage (plus twin overlay): stable entity IDs ↔ prim paths, spatial extents, binding index, layer/tag projections, and **geometry / material / hierarchy / metadata defaults**. May describe millions–billions of prims. Self-sufficient after import — no live USD dependency. Does not hold live metric values.                                                                                                |
| **Runtime Scene Graph (RSG)**                                         | Working set of entities currently *activated* by subscriptions. Holds resolved geometry handles (from the VF geometry store seeded at import), cached telemetry values, LOD choices, transient interaction state, and **telemetry-driven overrides that scale/transform the geometry, material, hierarchy, and metadata defaults** — renderer-independently. Sized for thousands of entities per process / per session pool.                                               |
| **Render Scene**                                                      | GPU-resident objects owned by a specific rendering backend. Sized for hundreds–thousands of visible / near-visible entities per viewer. Disposable; rebuilt from RSG (+ USD geom hydration).                                                                                                                                                                                                                                                                               |
| **Scene Graph Service (SGS)**                                         | Process that indexes USD into the LSG, computes interest, materializes / dematerializes the RSG, and exposes the Renderer Bridge + write-back APIs. **Runtime state engine.**                                                                                                                                                                                                                                                                                              |
| **Interest / Subscription Manager**                                   | Layer (inside SGS) that decides *what* must be active given camera AOI, selection, alerts, AI queries, and automation rules — analogous to virtual memory, MMO interest management, and GIS tile streaming. Maps cleanly onto USD payloads / LOD.                                                                                                                                                                                                                          |
| **Telemetry Binding**                                                 | Declarative link from an entity/prim attribute to an external resolver. Authored in USD (VF schema) or twin overlay; **values** never written back into USD on the hot path.                                                                                                                                                                                                                                                                                               |
| **Telemetry Resolver**                                                | Lazy fetcher that turns bindings into values only for active subscriptions. Supports batching, caching, expiration, and prioritization. Day-one metrics path queries **VictoriaMetrics** (PromQL HTTP), not a pile of per-source parsers.                                                                                                                                                                                                                                  |
| **VictoriaMetrics**                                                   | Default metrics TSDB / scrape + query backend. Scrapes Prometheus-format targets itself; accepts historical CSV/IoT import. Keeps the VectorFlow resolver thin.                                                                                                                                                                                                                                                                                                            |
| **Twin Overlay**                                                      | Small durable store (SQLite) for runtime-authored opinions that should not thrash the USD asset layers: committed pins, locks, binding overrides, compile stamps. Overlay opinions sit at the **top** of the resolution order (authored default < telemetry override < pin (pending or committed), §4.1) — the overlay adds *durability* to a committed pin, not extra precedence. Optional USD session layer export is a slow-path interchange step — not the live store. |
| **Renderer Bridge**                                                   | Engine-specific adapter that consumes a versioned, engine-neutral bridge API and maintains the Render Scene as a cache. May use native USD loaders for geometry; still applies live visual/transform diffs from SGS, not from mutating the stage every tick.                                                                                                                                                                                                               |
| **AOI (Area of Interest)**                                            | Spatially scoped interest region (typically from a camera frustum + margin). One of several subscription triggers — not the only one.                                                                                                                                                                                                                                                                                                                                      |
| **Snapshot Store**                                                    | Persistence for twin overlay + indexes; USD asset layers remain files/object storage. Not for high-frequency telemetry.                                                                                                                                                                                                                                                                                                                                                    |
| **Full-control / operator viewer**                                    | High-fidelity SSR session: independent camera + full interaction (pick/pin). Target: **~100** concurrent per deployment.                                                                                                                                                                                                                                                                                                                                                   |
| **Viewer profile**                                                    | Declared class (`operator` / `engineer` / `observer`) with count target, rendering mode, and interaction level — see §2.5.                                                                                                                                                                                                                                                                                                                                                 |
| Part / Pipe / Anchor / Tick / Pin-Unpin / Orchestrator / Overlay Hint | Same operational meanings as earlier versions; ownership clarified below.                                                                                                                                                                                                                                                                                                                                                                                                  |


---

## 1. Goals, Scope & Non-Goals

### 1.1 Goals

Build an open, renderer-agnostic digital twin runtime that supports:

- **Massive worlds** (order of **10⁷+ assets**, toward 10⁹ prims) via OpenUSD composition and payloads — logical scale far beyond GPU memory
- **Selective high-fidelity viewers** (~100 active 3D operators consuming GPU) — not “massive viewers”
- Live telemetry visualization without storing the live universe in RAM or rewriting the USD stage
- Large observer and API audiences without per-user SSR
- Browser delivery via streaming and/or client-side WebGPU
- AI-assisted exploration as a first-class interest producer
- Multiple rendering backends behind one bridge API
- OpenUSD as the standard geometry / scene-description interchange layer

### 1.2 In scope

- OpenUSD as authored scene description + geometry (with VF binding schemas)
- SGS as runtime state engine (interest, RSG, telemetry cache, pins)
- Lazy telemetry resolution with caching / batching
- Renderer-independent bridge API (geom hydration may use native USD loaders)
- Twin overlay persistence for overrides; USD layers for assets
- Multi-viewer sessions with independent interest sets, gated by **viewer profiles**
- Streaming path for server-side backends (O3DE default for operators)
- Read-only / query paths for observers and telemetry/API consumers

### 1.3 Non-goals (v4.4)

- Using OpenUSD / Omniverse Nucleus as a live telemetry bus or per-tick state store
- Building a general-purpose distributed database or multi-region scene fabric on day one
- Ingesting and retaining full-resolution historical telemetry **inside SGS** (that is VictoriaMetrics’ job; SGS only caches active subscription values)
- Requiring a Prometheus **server** in front of VictoriaMetrics for greenfield deploys
- Treating any renderer as authoritative for entity identity, topology, or pins
- Requiring Kafka / NATS / Redis / Postgres unless a measured need appears
- Perfect pixel-accurate picking in the SGS (coarse pick is enough; bridges may refine)
- Requiring full Omniverse stack (Nucleus, Kit, Connector ecosystem) — USD the format, not the Omniverse product suite
- Optimizing to **render** a billion users — world cardinality and render sessions must scale **independently** (see §2.5)

---

## 2. Architecture Overview

### 2.1 Core insight (v3 → v4.3)


| Prior assumption                                                        | Correction                                                                                                                                                                                        |
| ----------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| SGS holds live attribute values updated by a global telemetry event bus | SGS holds **bindings**; values exist only in the RSG for subscribed entities                                                                                                                      |
| AOI ≈ “what the camera sees”                                            | AOI is one **interest source**; selection, alerts, AI, and rules also activate                                                                                                                    |
| Scene graph ≈ what the renderer needs                                   | Scene graph has three tiers: **logical / runtime / render**                                                                                                                                       |
| Data Integration Layer pushes everything upstream                       | Resolvers are **pull/activate-on-demand**; push only for alert-class signals                                                                                                                      |
| USD (Omniverse-style) as live replicated world state                    | **OpenUSD = initialization / import format (not a runtime dependency)**; **SGS = runtime state engine** that owns geometry/material/hierarchy/metadata defaults and may scale them from telemetry |
| “Massive viewers” all need high-fidelity 3D                             | **Massive world, selective high-fidelity viewers** — GPU only for who needs full simulation                                                                                                       |


### 2.2 What OpenUSD is — and is not


| OpenUSD **is**                                                                                 | OpenUSD **is not**                                                           |
| ---------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| An **initialization / import** source of geometry, material, hierarchy & metadata **defaults** | A **runtime dependency** — the live runtime never opens or queries the stage |
| Hierarchy, composition (references, payloads, variants, layers)                                | The interest / subscription system                                           |
| Geometry, materials, lights, cameras (authored)                                                | The live telemetry value store                                               |
| Declarative VF schemas for bindings / semantic tags                                            | Per-tick deform / color / pin writes                                         |
| Asset interchange across DCC tools and renderers                                               | Per-viewer AOI or session state                                              |
| Natural LOD/streaming unit via **payloads** (imported into the VF geometry store)              | The Renderer Bridge protocol                                                 |


**Rule of thumb:** USD is read at **initialization / republish** to seed defaults; it is not consulted on the runtime hot path. If it changes at human authoring frequency (or on commit), it may live in USD or the twin overlay. If it changes at sensor / interaction frequency, it lives in the RSG and reaches renderers via the bridge API. Once seeded, the Scene Graph may **scale/transform** geometry, material, hierarchy, and metadata defaults from telemetry — renderer-independently, with no USD dependency.

### 2.3 Diagram

```
 ┌──────────────────────────────────────────────────────────────────────────┐
 │                    Content & Data Sources                                │
 │  CAD/BIM → OpenUSD layers/payloads                                     │
 │  Metrics: exporters / IoT → VictoriaMetrics (scrape + CSV import)        │
 │  Alerts (narrow): MQTT · webhooks · …                                    │
 └───────────────┬─────────────────────────────────┬────────────────────────┘
                 │ stage / assets                  │ resolve on demand
                 ▼                                 ▼
 ┌──────────────────────────────┐    ┌──────────────────────────────────────┐
 │ OpenUSD (scene description)  │    │ Telemetry & Asset Resolvers (Rust)   │
 │ prims · geom · materials     │    │ adapters · batch · cache · TTL       │
 │ payloads/LOD · VF bindings   │    │ alert push (narrow, not all metrics) │
 │ (files / object storage)     │    └──────────────────┬───────────────────┘
 └───────────────┬──────────────┘                       │
                 │ index / compose                      │
                 ▼                                      │
 ┌──────────────────────────────────────────────────────────────────────────┐
 │                     Scene Graph Service (Rust)  ← runtime state engine   │
 │                                                                          │
 │  ┌──────── Logical Scene Graph (index over USD + twin overlay) ────────┐ │
 │  │ entity↔prim map · extents · binding index · tags/layers · revision  │ │
 │  │ Twin Overlay / Snapshot Store (SQLite): pins, locks, overrides      │ │
 │  └─────────────────────────────────────────────────────────────────────┘ │
 │                                   │                                      │
 │                                   ▼                                      │
 │  ┌──────────────────── Interest / Subscription Manager ────────────────┐ │
 │  │ triggers: camera AOI · selection · alerts · AI queries · rules      │ │
 │  │ activates USD payloads / LOD + telemetry bindings                   │ │
 │  └─────────────────────────────────────────────────────────────────────┘ │
 │                                   │                                      │
 │                                   ▼                                      │
 │  ┌──────────────────── Runtime Scene Graph (working set) ──────────────┐ │
 │  │ activated prims · hydrated geom · cached telemetry · drag/pins      │ │
 │  │ shared pages + per-subscriber diffs  (NOT written into USD stage)   │ │
 │  └─────────────────────────────────────────────────────────────────────┘ │
 │                                                                          │
 │  APIs: subscribe · write-back · pick · query · USD/DSL reload            │
 └───────────────────────────────┬──────────────────────────────────────────┘
                                 │ Renderer Bridge API (vf.bridge.v1)
                                 │ diffs: xform / visual / lod + usd geom refs
                                 ▼
 ┌──────────────────────────────────────────────────────────────────────────┐
 │ Renderer Bridge  →  Render Scene (GPU cache)                             │
 │ hydrate meshes from USD payloads; apply live state from bridge diffs     │
 │ O3DE Gem (default) · Unreal plugin · WebGPU client                       │
 └───────────────────────────────┬──────────────────────────────────────────┘
                                 │
              ┌──────────────────┼──────────────────┐
              ▼                  ▼                  ▼
        Orchestrator      Streaming Layer     Browser Client
     (SSR workers)     (Wilbur+SFU / WS)   (video + overlay UI)
```

### 2.4 Scale model (design envelope)

**World and sessions scale on different axes:**

```
 ~10,000,000 assets (logical world / OpenUSD)
              │
              │  interest / subscriptions
              ▼
 ~100 active 3D operators          ← server GPU, full interaction
              │
              ▼
 ~10,000 observers                 ← WebGPU / video / dashboards, read-only
              │
              ▼
 ~100,000 telemetry / API users    ← queries & bindings, little or no 3D
```


| Tier                  | Typical cardinality                                | What lives there                                | Cost driver                                |
| --------------------- | -------------------------------------------------- | ----------------------------------------------- | ------------------------------------------ |
| OpenUSD / Logical     | **~10⁷ assets** baseline; toward 10⁹ prims         | Composition, geom refs, bindings, extents index | Disk + payload I/O; sparse indexes         |
| Runtime               | 10³ – 10⁴ active entities (shared across sessions) | Hydrated geom + cached telemetry + interaction  | CPU / RAM / resolver QPS                   |
| Render (per operator) | 10² – 10³ GPU objects                              | Meshes, materials, deformed instances           | **GPU memory + encode** (scarce)           |
| Observer / API        | 10⁴ – 10⁵ clients                                  | Shared views, dashboards, telemetry reads       | Bandwidth / API QPS — **not** per-user SSR |


**Key design decision:** Do not optimize VectorFlow3D to render a billion users. Optimize it so a **billion objects can exist** while only users who need full simulation consume GPU resources. Aligns with MMO interest management, GIS tile streaming, and industrial SCADA-scale read fan-out.

**How cost drops:** Interest Manager activates USD payloads and bindings only for AOI / selection / alerts / AI / rules. LOD and visibility shrink each Render Scene. Telemetry resolves only for active bindings. Inactive assets cost near-zero beyond index presence. **Growing the world does not require growing the GPU farm**; growing GPU farm only follows growth in **operator** (and limited engineer SSR) sessions.

Analogy:

- **Virtual memory:** USD stage = address space; RSG = resident set; Render Scene = GPU-side cache.
- **MMO interest management:** each high-fidelity client has an interest set; most players never get a dedicated sim blade.
- **GIS tile streaming:** the map is planetary; only the viewport tiles are hot.

### 2.5 Viewer profiles (locked architecture target)

```yaml
viewer_profiles:

  operator:
    count_target: 100
    rendering: server_gpu          # O3DE SSR (default) / optional Unreal
    interaction: full              # independent camera, pick, pin
    notes: "High-fidelity 3D; one Render+Encode Worker per session"

  engineer:
    count_target: 1000
    rendering: hybrid              # mix SSR (scarce) + WebGPU / follow / shared AOI
    interaction: limited           # inspect, measure, limited edits per policy
    notes: "Must not assume 1000 SSR workers; prefer bridge clients + shared views"

  observer:
    count_target: 100000
    rendering: WebGPU | video | dashboard
    interaction: read_only
    notes: "No per-user full simulation; may follow operator cameras or fixed shared views"

# Adjacent non-3D audience (same SGS / resolvers, not a viewer_profile):
# telemetry_api_users:
#   count_target: 100000
#   access: query / subscribe bindings / alerts — little or no Render Scene
```


| Profile               | Count target | Rendering                  | Interaction            | Consumes SSR GPU?                                  |
| --------------------- | ------------ | -------------------------- | ---------------------- | -------------------------------------------------- |
| `operator`            | **100**      | `server_gpu`               | `full`                 | **Yes** — size pool here                           |
| `engineer`            | **1,000**    | `hybrid`                   | `limited`              | **Sometimes** — budget explicitly, default off SSR |
| `observer`            | **100,000**  | WebGPU / video / dashboard | `read_only`            | **No**                                             |
| Telemetry / API users | **~100,000** | N/A (API)                  | read / alert subscribe | **No**                                             |


**v1 shipping focus:** prove **~10M-asset world** + **~100 operators** on SSR. Engineer hybrid and observer paths must stay API-compatible from day one (`vf.bridge.v1` + query APIs) even if observer UX ships after operator SSR.

**Cost implication:** GPU farm sized for **~100** full-simulation sessions, not for observers or API users. World growth (assets) is an **indexing / payload** problem; audience growth is a **profile routing** problem.

**What we are not designing for:** “massive viewers” each with personal high-fidelity SSR. That conflates world scale with render scale and fails the same way games fail when every client gets a dedicated sim server.

---

## 3. Components

### 3.0 OpenUSD — initialization / import format (not a runtime dependency)

OpenUSD is the **initialization / import substrate** — read to seed the Scene Graph, **not** a runtime dependency:

- Prim hierarchy and composition arcs (references, payloads, variants, layer stacks)
- Geometry and materials (including LOD via payload / purpose / variant conventions)
- Authored transforms and static/BIM metadata
- VectorFlow **custom schemas** for declarative telemetry bindings and semantic tags (API schema style — attributes describe *how* to resolve, not live values)
- Asset identity via content-addressable layer URIs

All of the above are consumed **once, at import/reindex time**, to establish geometry, material, hierarchy, and metadata **defaults** in the VectorFlow Scene Graph. After import the Scene Graph is authoritative and self-sufficient.

**SGS relationship to USD:**

1. **Import at initialization / republish:** compose the stage once (or a lightweight metadata pass over payload bounds) to extract geometry, materials, hierarchy, transforms, and metadata **defaults** into VF-owned stores. This is the **only** point USD is required.
2. Build the LSG index: `EntityId ↔ Sdf.Path`, extents, binding descriptors, tags, and default transforms/materials.
3. On interest activation, load geometry from the **VF geometry store** (seeded from USD at import) — **not** by re-opening or querying a live USD stage.
4. At runtime the Scene Graph is **self-sufficient**: it may **scale/transform** geometry, materials, hierarchy, and metadata defaults from telemetry (and other inputs), renderer-independently, without any USD dependency.
5. Never write high-frequency telemetry or ephemeral AOI into USD.
6. On **committed** pin/override: write to Twin Overlay (SQLite) immediately; optionally export a USD session/opinion layer as a slow-path interchange artifact for DCC round-trip.

**Why not “USD as runtime”:** USD is an initialization format, not a live state machine. Writing live attributes into a composed stage at sensor rates couples composition cost to telemetry QPS, fights multi-viewer isolation, and turns asset layers into a database. USD composition is excellent for authored worlds and for **bootstrapping** the twin; it is a poor hot-path runtime. Keep Omniverse Nucleus / live stage sync **out** of the critical path unless a deployment explicitly needs DCC co-editing — and even then, co-edit the *authored* layers on the slow path, not the telemetry stream.

**Library choice:** Prefer OpenUSD (Apache 2.0) via existing bindings/FFI from the Rust SGS (or a small C++ helper process) for **import-time** composition and geometry extraction. Because USD is only an **initialization** dependency, this FFI is **not on the runtime hot path**. Do not take a dependency on Omniverse Kit for core runtime.

> **Implemented (Phase 1):** the "small helper process" option was chosen over in-Rust FFI. A Python helper (`[sgs/tools/usd_export.py](sgs/tools/usd_export.py)`) using the `usd-core` wheel composes the stage with `**Usd.Stage.Open(root, load=Usd.Stage.LoadNone)`** (payloads never opened) and streams **NDJSON, one record per prim**, which the Rust SGS (`[sgs/](sgs/)`) ingests into the LSG. This keeps OpenUSD entirely off the Rust build and off the runtime hot path, and sidesteps the arm64 OpenUSD-C++ build blockers on this platform. Because payloads stay unloaded, component-internal metadata authored *inside* the payload (e.g. `kind`, `class`) is not visible at index time; a component instance is identified by the presence of a `payload` arc (its `kind` is taken as `component`) plus instance-authored `customData.vf` (`assetTag`, `tags`). Component defaults that must be indexed without live geometry are surfaced on demand by the **Phase 2** payload-load path: the same helper's `--payload` mode opens a component payload on interest activation to reveal its internal `kind` / `customData.vf.class` / geometry bbox, cached by `content_hash` and held **only in the RSG** (never written back to the LSG or USD) — see Phase 2 in §7.

**VF authoring convention (pre-v1, locked v4.7).** Until a codeless `VfTelemetryBindingAPI` is registered, VectorFlow assets author bindings and identity as a stable, forward-compatible convention (reference implementation: `[assets/usd/pump-station-01/](assets/usd/pump-station-01/)`):

- **Identity / tags / static metadata** → `customData.vf` dictionary (`class`, `assetTag`, `tags[]`, plus static fields such as `manufacturer`, `model`). Dictionary-valued metadata composes key-by-key, so reusable component defaults merge with per-instance opinions.
- **Telemetry bindings** → `vf:binding:<attribute>:*` namespaced `custom` attributes: `sourceId` (`"victoriametrics"`), `query` (fully-resolved PromQL), `attribute`, `unit`, `ttlMs`, `priority` (`background` | `high`), `qualityPolicy` (`stale_ok`). Each binding describes *how to resolve* a value — never the value itself.
- **Bootstrap composition:** a facility **assembly** (kind `assembly`) → area **groups** (kind `group`) → **components** (kind `component`) deferred behind `**payload`** arcs (the interest / streaming quantum, §3.2). Instance prims carry exactly the light, index-friendly data the LSG needs *without loading payloads*: transform, `extentsHint`, `customData.vf` identity/tags, and the bindings. Stage conventions: **Z-up**, `metersPerUnit = 1`.

### 3.1 Logical Scene Graph (index, not a second geometry DB)

The LSG is SGS’s **authoritative index**, initialized from OpenUSD + Twin Overlay at import. The USD-derived values below are **defaults** the runtime (RSG) may scale/override from telemetry:

- `EntityId` ↔ prim path (stable hash of prim path / asset id)
- Parent/child as projected from USD hierarchy at import (composition already applied for indexing); a **default** hierarchy the runtime may re-parent/scale
- Authored transform from USD as a **default**; **committed** pin/override from Twin Overlay (stronger opinion at runtime)
- Metadata / tags projected from USD + VF schemas as **defaults**
- `GeomRef` = handle into the VF geometry store (imported from a USD payload/layer at init) + content hash + LOD ladder
- Telemetry bindings projected from VF schemas (still declarative)
- Coarse spatial extents for interest queries (from USD bounding boxes / authored extents at import)

Does **not** own:

- Live metric time series
- GPU resources
- Per-viewer camera / interest state
- A duplicate mesh database parallel to USD (avoid forked truth)

**Challenge to v3/v4-draft:** “LSG owns geometry references” without naming USD left interchange under-specified. USD is the geometry **init source**; after import the VF geometry store is the runtime truth and the LSG is the queryable index over it — no live USD dependency.

### 3.2 Interest / Subscription Manager

Central control plane for activation. Every consumer registers a **Subscription**:

```text
Subscription {
  id, subscriber_kind,            // viewer | ai | alert_rule | automation | system
  viewer_profile?,                // operator | engineer | observer (when kind=viewer)
  spatial?: { frustum | sphere | aoi_id },
  entity_ids?: [...],             // explicit selection / query hits
  tags_filter?: [...],
  lod_policy, priority, budget,   // max entities / bytes / resolver QPS / payload MB
  interests: { geometry, metadata, telemetry, overlay },
  ttl / heartbeat
}
```

Orchestrator + Interest Manager enforce profile budgets (e.g. `observer` cannot request `interaction: full` or allocate `server_gpu`).
**Triggers that create or reshape subscriptions:**


| Trigger          | Typical effect                                                        |
| ---------------- | --------------------------------------------------------------------- |
| Camera AOI       | Spatial activate + load USD payloads + LOD by distance / screen size  |
| User selection   | Keep entities in RSG regardless of frustum                            |
| Alerts           | Force-activate implicated prims + neighbors; raise telemetry priority |
| AI queries       | Temporary high-priority subscription over query result set            |
| Automation rules | Persistent background subscriptions (e.g., “all pumps in alarm”)      |


**Outputs:** activate, deactivate, priority change, LOD hint, payload prefetch (predictive margin around AOI).

**USD-specific note:** Treat payloads as the primary streaming quantum. Interest evaluation should decide *which payloads to open*, not only which leaf prims to mark visible.

**Design rule:** Prefer one Interest Manager inside SGS over a separate microservice. Cross-process interest only becomes justified if SGS itself shards.

### 3.3 Runtime Scene Graph

Materialized working set produced by applying subscriptions to the LSG index (initialized from USD):

- Entity snapshots for active IDs / prim paths
- Geometry handles (from the VF geometry store seeded at import — **not** re-read from a live USD stage)
- Telemetry value cache entries (`value`, `as_of`, `ttl`, `quality`)
- **Telemetry-driven overrides that scale/transform the USD-seeded defaults** — geometry (scale/deform/swap LOD), materials (color/emissive/state), hierarchy (re-parent/group), and metadata — computed renderer-independently and emitted as bridge diffs
- Transient interaction state (drag candidates, pending pins)
- Diff generation toward Renderer Bridges

**Critical invariant:** RSG state (including telemetry-driven geometry/material/hierarchy/metadata overrides) is **not** flushed into USD; USD is an import artifact, not a runtime store. Bridges see bridge diffs; DCC tools see USD (+ optional slow-path overlay export).

**Per-subscriber views vs shared RSG:** Share underlying entity pages (geometry + telemetry cache) across subscribers; maintain lightweight per-subscriber interest masks and diff cursors. Do **not** fork a full RSG copy per viewer — and do **not** fork a USD stage per viewer for live data.

**Eviction:** When no subscription references an entity (plus grace period), drop runtime state and may unload payloads. USD asset layers on disk untouched.

### 3.4 Telemetry & Asset Resolvers

Replace v3's always-on "normalize everything onto an event bus → update SGS attributes" with **resolve-on-activate**. Bindings may be authored on USD prims; resolution results land only in RSG.

#### Metrics backend (locked): VictoriaMetrics

**VictoriaMetrics** is the default store/query surface for time-series telemetry:


| Concern                     | Approach                                                                                                   |
| --------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Live scrape                 | VM scrapes Prometheus-format targets / exporters **directly** — **no Prometheus server required in front** |
| Resolver queries            | PromQL-compatible HTTP (`/api/v1/query`, `/api/v1/query_range`) against VM                                 |
| Historical CSV / IoT import | Ingest into VM (e.g. CSV import / `vmimport` / push protocols) **once**; resolver still only speaks PromQL |
| Why this split              | Import and retention complexity stay in VM; VectorFlow resolvers stay a thin batching client               |


This is attractive specifically when onboarding **historical CSV IoT data**: do not teach the SGS resolver to parse CSV layouts, clock skew, or backfill. Land data in VictoriaMetrics, bind prims to metric names/labels, resolve lazily for active subscriptions.

Optional: a standalone Prometheus server may still exist in some customer environments; the resolver should treat it as “another PromQL endpoint.” Prefer documenting **VictoriaMetrics-first** so greenfield deploys stay one metrics box.

#### Responsibilities

1. Map binding descriptors to PromQL (or other protocol) fetches
2. Batch requests (VictoriaMetrics instant/range queries; later OPC-UA multi-read, etc.)
3. Cache with TTL / stale-while-revalidate
4. Prioritize (selection + alerts > camera AOI background > prefetch)
5. Emit quality flags (`ok`, `stale`, `unavailable`, `error`)
6. Accept a **narrow push path** for alert / exception events that should wake interest without polling (MQTT or webhook) — alerts are not a substitute for the metrics TSDB

**Day-one connectors:**

1. **VictoriaMetrics** (PromQL) — live scrape + historical import path
2. One push/alert source (**MQTT** or webhook)

Add OPC-UA / Modbus / Kafka only when a deployment needs them — adapters are libraries behind one resolver trait, not a mandatory multi-broker platform. Do **not** put Prometheus-the-server on the critical path by default.

**What not to build yet:** a durable message bus between resolvers and SGS; a CSV parser inside the hot-path resolver; a second historian inside SGS. An in-process async queue to VM is enough until fan-out or multi-node SGS exists.

**Binding shape (metrics):** prefer `resolver: { source_id: "victoriametrics", query: "<promql>", … }` (or metric name + label matchers). Instant queries for live visual state; range queries only when a subscription explicitly needs a sparkline / scrubber — still not stored in LSG/USD. **In USD** this is authored per attribute as `vf:binding:<attribute>:{sourceId, query, attribute, unit, ttlMs, priority, qualityPolicy}` (the pre-v1 convention, §3.0); the PromQL `asset` label carries the prim's `vf.assetTag`, e.g. `pump_flow_gpm{asset="PUMP-01"}`. Seed sample or historical data **into VictoriaMetrics** — never into USD or the resolver — through VM's Prometheus / CSV import paths (dev helper: `[scripts/seed-victoriametrics.sh](scripts/seed-victoriametrics.sh)`).

### 3.5 Renderer Bridge API (engine-independent)

All backends consume the same versioned API. The bridge maintains a Render Scene as a **cache**:


| Direction    | Operations                                                                                                                                                                                                     |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| SGS → Bridge | `UpsertEntity`, `RemoveEntity`, `SetTransform`, `SetGeomRef/LOD` (VF geometry store handle + hash), `SetVisualState` (color/deform/geometry-scale from resolved telemetry), `SetOverlayHint`, `SnapshotMarker` |
| Bridge → SGS | `UpdateCamera` / `UpdateAOI`, `PickRequest`, `PinPart` / `UnpinPart`, `SubscribeExtras` (selection), heartbeat / budget                                                                                        |


**Geometry hydration:** Bridges hydrate meshes from the VF geometry store referenced by `GeomRef` (imported from USD at init). A bridge **may** use native OpenUSD loaders (O3DE USD Gem, Unreal USD, etc.) as an **import / asset-prep** convenience, but the live runtime path does **not** depend on an open USD stage. Live transforms, visual parameters, and telemetry-driven geometry/material/hierarchy overrides arrive as bridge diffs from SGS — **not** by the bridge polling a live-mutated stage.

**Transport:**

- Co-located SSR worker: shared memory ring buffer or Unix socket (preferred for latency)
- Remote / WebGPU (observer) client: **WebSocket** for the data-only `vf.bridge.v1` diff stream — bidirectional (`BridgeMsg` down, `BridgeRequest` up), browser-native, full-duplex
- SSR video (operator): WebRTC via Epic Pixel Streaming (Wilbur + SFU) — media track + DataChannel control (§3.7); unchanged
- Contract is the message schema + semantic versioning (`vf.bridge.v1`), not the transport

**Transport amendment (locked, 2026-07) — supersedes the prior "Remote / WebGPU client: gRPC or WebTransport" line.**
- **gRPC dropped for the browser path.** It does not run natively in a browser without a `grpc-web` shim/proxy, so it cannot serve an actual WebGPU client as written.
- **WebTransport deferred** pending a local-dev certificate story: HTTP/3 needs real TLS or `serverCertificateHashes` (ECDSA, ≤14-day cert lifetime) — real friction for self-signed local dev — and its multiplexed-streams/datagram advantage matters far less for an event-driven diff stream than for the video case it was designed around. It may return for the observer path once the cert/scale story justifies it.
- **WebSocket chosen** for the WebGPU/observer path: browser-native, full-duplex, carries the existing schema unchanged, and in Rust can stay blocking (thread-per-connection, no async runtime) to match the crate's no-tokio ethos.
- **WebRTC is not used for the data path** — it remains the SSR **video** transport only. §3.6's "No video path" for the WebGPU tier stands; adopting WebRTC here would mean a second WebRTC stack alongside Wilbur for zero video benefit.

**Hard rule:** Bridges never invent durable entity IDs, never persist pins locally as truth, never become the topology source, never treat a local USD stage as authoritative for live twin state. On reconnect they resync from SGS (snapshot + catch-up diffs).

### 3.6 Rendering backends


|           | O3DE (default SSR)                                                          | Unreal (optional SSR)                        | WebGPU (light / enterprise tier)                  |
| --------- | --------------------------------------------------------------------------- | -------------------------------------------- | ------------------------------------------------- |
| Role      | Full-control SSR + encode                                                   | SSR via Pixel Streaming                      | Client-side render from bridge stream             |
| License   | Apache 2.0 / MIT                                                            | EULA + royalty above $1M                     | Your code                                         |
| USD       | Import-time USD hydration → VF geom store (no runtime stage)                | Native USD plugins at import where available | Decode via SGS-provided meshes or wasm USD subset |
| Streaming | Build Streamer Gem; reuse Epic Wilbur (formerly Cirrus) + SFU for signaling | Full Pixel Streaming stack                   | No video path                                     |
| When      | `operator` profile: **~100** server_gpu                                     | Need mature streaming sooner                 | `engineer` hybrid + `observer` at scale           |


**Decision guidance (locked):** Default **O3DE `server_gpu`** for `operator` (~100). `engineer` defaults to hybrid (SSR only when explicitly granted). `observer` never allocates a Render+Encode Worker. Keep `vf.bridge.v1` identical across profiles so world scale and session scale stay decoupled. Prefer Unreal only if Pixel Streaming schedule outweighs royalty for a specific deployment.

### 3.7 Orchestrator & Streaming

- One Render+Encode Worker per `**operator**` session (`server_gpu`, independent camera). Size the pool for `**count_target: 100**` (§2.5). Shared-camera / follow modes may share a worker.
- `engineer` / `observer` / telemetry-API sessions do **not** get an SSR worker by default; they subscribe to SGS with profile-appropriate budgets and rendering modes.
- Worker crash is cheap: LSG index + Twin Overlay live in SGS; worker resubscribes and rebuilds Render Scene from USD payloads + bridge diffs.
- Reuse Epic Pixel Streaming Infrastructure (**Wilbur** — the signalling server formerly named *Cirrus* — **+ SFU**; skip deprecated Matchmaker) for signaling/SFU regardless of O3DE vs Unreal streamer implementation. Deploy via Epic's official container images (locked to a single UE line so signalling protocol versions match).
- **SFU networking caveat (locked):** the SFU image requires Docker **host networking** (Epic's own requirement — the SFU discovers/reports its own WebRTC ports), which is effectively **Linux-only** and the standard path for CI/prod. macOS dev runs the SFU as a **native process** against the containerized signalling server, or skips the SFU entirely (a single viewer connects directly to the streamer).
- **Reference streamer + FrameSource seam (locked):** a permanent **reference streamer** pushes synthetic frames (color bars, moving element, overlaid timestamp + **monotonic frame `seq`**, plus a machine-readable marker) over the real WebRTC path **before** any GPU renderer exists. It sits behind an explicit `**FrameSource`** interface (raw pixel buffers / GPU texture handles); the O3DE Streamer later replaces **only** that implementation — encode, transport, and signalling stay unchanged. A headless **CI harness** subscribes as a player and asserts drops / reordering / latency from the `seq` marker. **Scope limit:** synthetic low-entropy frames validate the signalling/transport/protocol path only; they do **not** stress the hardware encoder the way real CAD geometry does, so a green harness proves plumbing — it is **not** a substitute for real O3DE-sourced end-to-end validation (a separate later gate, Phases 6–7).
- WebRTC media for video; DataChannel for control messages (`POINTER_EVENT`, `CAMERA_DELTA`, `PICK_RESULT`, `PIN_CONFIRM`, `OVERLAY_HINT`, …).
- WebSocket video fallback is **not** provided by Wilbur — treat as an explicit product decision (build MJPEG/WS or accept WebRTC-only).

### 3.8 Browser Client

Video/WebCodecs display (SSR path), Overlay UI from hints, drag-ghost prediction for latency masking, input capture. Unchanged in spirit from v2/v3; still renderer-agnostic.

### 3.9 Flow3D DSL and USD authoring

Two complementary authoring paths — both feed **scene description**, not runtime:


| Path                | Produces                                                                         | Use when                                           |
| ------------------- | -------------------------------------------------------------------------------- | -------------------------------------------------- |
| DCC / CAD → OpenUSD | Layers, payloads, materials, prim hierarchy                                      | Large BIM/CAD imports, vendor assets               |
| Flow3D DSL          | Twin semantics: pipes/anchors, bindings, tags; may emit/patch USD + Twin Overlay | Declarative twin wiring, automation-friendly edits |


DSL compiles into LSG opinions (and optionally USD layers) with stable IDs. It never emits live telemetry values or renderer-specific objects. Incremental recompile patches the index; Interest Manager re-evaluates affected subscriptions.

> **Implemented (Phase 4):** the Flow3D DSL ships as a hand-written lexer/parser/compiler (`[sgs/src/dsl/](sgs/src/dsl/)`) with line/column caret diagnostics. It lowers `.flow3d` source to durable Twin-Overlay `[Opinion](sgs/src/opinion.rs)`s and patches the LSG **in place** (stable IDs; **no USD writes** — vendor layers stay byte-identical). Incremental recompile diffs against persisted `compile_stamps` and applies only the delta, so re-evaluating the same interest set emits no activate/deactivate storm. See Phase 4 in §7 for the locked choices and reference `[assets/flow3d/pump-station-01.flow3d](assets/flow3d/pump-station-01.flow3d)`.

---

## 4. Data Model

### 4.1 Ownership split


| Concern                                                         | System of record                                                                     |
| --------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| Prim hierarchy, geom, materials, authored xforms (**defaults**) | OpenUSD layers at **import/init** → VF Scene Graph (LSG + geometry store) at runtime |
| Declarative telemetry bindings / semantic tags                  | OpenUSD VF schemas (preferred, imported) or Twin Overlay                             |
| Committed pins, locks, binding overrides                        | Twin Overlay (SQLite)                                                                |
| Telemetry-scaled geometry / material / hierarchy / metadata     | RSG (renderer-independent overrides of the USD-seeded defaults)                      |
| Live telemetry values, AOI, drag-in-flight                      | RSG only                                                                             |
| GPU meshes / materials instances                                | Render Scene only                                                                    |


**Precedence (total order, locked).** Earlier versions stated precedence only pairwise (`overlay > authored USD xform` here; `pin > telemetry` in Phase 8). For any attribute where an authored default, a telemetry-driven override, and a pin could all apply at once, the single resolution chain is:

> **authored USD default  <  telemetry-driven override (RSG)  <  pin (pending or committed)**

That is: the RSG scales/transforms the USD-seeded default from telemetry, and a **pin overrides both** (a human/automation opinion wins over a live reading, which wins over the static default). **Pending** pins (transient, drag-in-flight, RSG-only) and **committed** pins (Twin Overlay) — the two states §3.3 distinguishes — sit at the **same precedence rank**; only their *durability* differs, not their precedence. This matters mid-gesture: if a pending drag ranked below the committed-pin tier, a live telemetry tick could visually fight the user's drag before it is ever committed — exactly the failure mode the drag-ghost prediction in §3.8 exists to prevent. Precedence holds per attribute — a pin on `transform` does not freeze an unrelated telemetry-driven `color`. Enforcement lands with pins (Phase 8); it is stated here because the Flow3D DSL (Phase 4) is the first component to author overlay opinions and must resolve against one unambiguous order.

### 4.2 Core types

```text
PrimPath        = Sdf.Path (USD)
EntityId        = stable hash(prim_path | asset_id)
Transform       = authored (from USD) + optional pinned override { transform, pinned_by, at }  // override in Twin Overlay
GeomRef         = { usd_layer_or_payload_uri, content_hash, lod_ladder[], bbox, prim_path }
TelemetryBinding = {
  attribute,           // e.g. temperature, pressure, deform_amp
  resolver: { source_id, external_key, params },
  map: expression?,    // unit convert / clamp / to visual param
  ttl_ms, priority, quality_policy
}  // authored on prim as vf:binding:<attribute>:{sourceId,query,attribute,unit,ttlMs,priority,qualityPolicy}
   // (pre-v1 convention, §3.0); asset label = vf.assetTag; values NOT stored on prim
Entity = {
  id, prim_path, parent?, children[], tags[],
  transform, metadata,
  geom?: GeomRef,
  bindings: TelemetryBinding[],
  extents: AABB,
  flags: { selectable, deformable, … }
}
Edge / Pipe     = connectivity between anchors (may be USD relationships or Twin Overlay)
Scene           = { root_layer, entities_index, edges, layers, revision }
```

### 4.3 What is *not* on the USD prim / LSG entity

- `live_value` / per-tick metric readings
- GPU mesh pointers
- Per-viewer visibility flags
- Session cameras / interest sets

Those belong in RSG / Render Scene.

### 4.4 Runtime record (RSG)

```text
RuntimeEntity = {
  id, prim_path,
  lsg_revision,
  geom_handle?,          // hydrated from USD payload
  lod_level?,
  telemetry: Map<attribute, { value, as_of, ttl, quality }>,
  subscribers: set<SubscriptionId>,
  visual_params  // derived view used for diffs to bridges — not written to USD
}
```

### 4.5 Persistence

**Persist in OpenUSD (asset store):**

- Geometry, materials, hierarchy, composition, authored metadata
- VF binding schemas (declarative)
- Optional slow-path export of committed override layers for DCC round-trip

**Persist in Twin Overlay / Snapshot Store (SQLite, single-node default):**

- Committed pins / transform overrides / soft locks
- Binding overrides that must not edit vendor asset layers
- Entity↔prim index checkpoints / scene revision / DSL compile stamps
- Asset registry extras (resolver source configs — secrets in a secrets store, not USD)

**Persist in VictoriaMetrics (metrics TSDB):**

- Live scraped series and imported historical CSV / IoT time series
- Retention / downsampling per VM config — **not** SGS’s problem

**Do not persist by default:**

- High-frequency telemetry values **inside SGS / Twin Overlay / USD** (only short-lived RSG cache for active subscriptions)
- RSG caches, geom handles, Render Scene
- Per-viewer cameras / ephemeral AOIs
- Live attribute spam into USD layers

**Write pattern:** debounced / batched Twin Overlay writes; never USD composition on the interactive drag hot path. Drag updates RSG + bridge diffs; commit flushes overlay.

**Migration trigger:** move off SQLite only when SGS must shard or multi-writer HA is a measured requirement — not preemptively. USD itself already scales as files/object storage + payloads.

### 4.6 Concurrency model

- USD asset layers: treat as immutable or versioned publishes; SGS reindexes on new revision.
- Twin Overlay mutations (pin/unpin, metadata edits) go through a single-writer revision counter.
- Readers (interest evaluation, bridge diffs) see a committed revision.
- Multi-user pins: soft locks per entity (`lock(entity, user, ttl)`) default for interactive editing; LWW for automation — choose per deployment.
- Telemetry cache updates are lock-free / sharded maps keyed by `EntityId`.
- Do **not** use USD layer muted/unmuted opinions as the multi-user locking mechanism for live ops.

---

## 5. API Surfaces


| Boundary                 | Style                             | Payload                                               |
| ------------------------ | --------------------------------- | ----------------------------------------------------- |
| USD assets → SGS         | compose / reindex on publish      | root layer URI + revision                             |
| Interest consumers → SGS | subscribe / heartbeat / cancel    | Subscription descriptors                              |
| Resolvers ↔ SGS          | in-process async (default)        | `ResolveRequest(bindings[], priority)` → values       |
| SGS → Renderer Bridge    | versioned stream (`vf.bridge.v1`) | RSG diffs + USD `GeomRef`s (not live stage mutations) |
| Bridge → SGS             | request/response                  | camera/AOI, pick, pin/unpin, selection                |
| Browser (observer WebGPU) ↔ SGS | WebSocket (`vf.bridge.v1`) | diffs down; camera/pick/pin up (§3.5 amendment; Phase 5.5) |
| Observer ↔ SGS (geometry) | WebSocket binary frame (`vf.bridge.v1`) | `FetchGeom{content_hash}` up; GLB mesh bytes down (content-addressed VF geometry store; Phase 5.6) |
| Browser ↔ Streaming      | WebRTC DataChannel (+ media)      | input, overlay, confirms                              |
| External admin / AI      | HTTPS + authz                     | query, subscribe, USD/DSL reload                      |


**API versioning:**

- Bridge protocol: `vf.bridge.vN` with additive fields first; breaking changes bump N; bridges negotiate on connect.
- Scene revision: monotonic; includes USD root-layer revision + Twin Overlay revision.
- VF USD schema version separate from bridge protocol version.
- DSL language version separate from both.

---

## 6. Production Concerns

### 6.1 Failure recovery


| Failure                       | Expected behavior                                                                    |
| ----------------------------- | ------------------------------------------------------------------------------------ |
| Resolver outage               | Serve stale cache with `quality=stale`; degrade visuals; do not mutate USD           |
| Missing / corrupt USD payload | Mark geom `unavailable`; keep bindings/metadata if indexed; do not crash SGS         |
| SGS crash                     | Reload Twin Overlay + reindex USD root; RSG cold; bridges resubscribe                |
| Worker / bridge crash         | Orchestrator replaces worker; resync snapshot + diffs; pins already in overlay       |
| Streaming disconnect          | Client reconnects signaling; worker may be sticky-reused; state not in video session |
| Partial DSL / USD publish     | Reject transaction; keep last good scene revision                                    |


### 6.2 Security boundaries

- Authenticate browser sessions at Orchestrator / signaling edge
- Authorize entity access (tags / layers / tenants) **in Interest Manager** so AOI cannot leak out-of-tenant geometry or metadata
- Gate USD asset URLs the same way — bridges receive only authorized `GeomRef`s
- Resolvers hold source credentials; bridges and browsers never see them
- Pin/write-back requires edit capability; read-only viewers get subscribe + pick only
- Treat USD publish, DSL upload, and admin APIs as privileged
- Do not expose a writable live USD stage to browsers or untrusted agents

### 6.3 Observability

Export at minimum:

- Subscription count, activated entity count, open USD payload count, RSG RAM
- Resolver QPS, batch size, cache hit ratio, stale ratio
- Bridge diff backlog / apply latency
- Glass-to-glass latency (SSR), input-to-pin RTT
- Twin Overlay write lag; scene revision (USD rev + overlay rev)
- Hot-path USD write count (**should be ~0 under live load**)
- Per-tier cardinality gauges (logical / runtime / render)

Trace IDs should flow: client input → SGS mutation → bridge diff → frame (where applicable).

### 6.4 Testing strategy


| Layer           | Tests                                                                                         |
| --------------- | --------------------------------------------------------------------------------------------- |
| LSG / USD index | property tests for prim↔entity map, revisions, pin precedence over authored xform             |
| Interest        | deterministic fixtures: AOI + selection + alert → expected activate set + payloads            |
| Resolvers       | contract tests per adapter; chaos (timeouts, partial batches)                                 |
| Bridge          | golden diff streams against a fake renderer; geom refs point at fixture USD                   |
| E2E             | headless worker + mock client; pin survives worker kill; USD assets immutable during live run |
| Load            | see §6.5                                                                                      |


### 6.5 Performance benchmarks (gate Phases)

Target envelopes (tune per deployment; measure before optimizing):


| Metric                                             | Initial gate                                                                                                                                                                                                                                                                                                                                                                                                     |
| -------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| LSG/USD index of 10M prim refs (payloads unloaded) | cold reindex < 30s on target hardware; RSS bounded — **Phase 1 baseline: ~19s / ~5.7 GiB** via `sgs synth --count 10000000`                                                                                                                                                                                                                                                                                      |
| Interest eval @ 60 Hz camera updates               | < 2 ms p99 for 50k candidates → ≤5k activate — **Phase 2 baseline: ~0.3–1.0 ms/step** over a 10M-entity synth world (grid broad-phase, `sgs interest`)                                                                                                                                                                                                                                                           |
| Payload activate                                   | p95 open+hydrate within budget for target LOD                                                                                                                                                                                                                                                                                                                                                                    |
| Resolver batch                                     | ≥ 1k bindings/refresh within TTL budget — **Phase 3 baseline: 1000 synth bindings collapse to 3 metric-batched queries; the 29-binding fixture → 12 queries** (`sgs resolve`), stub and VictoriaMetrics behind one `Resolver` trait. Each batched query is capped at `MAX_ASSETS_PER_QUERY` (512) assets so automation-rule-scale activations split into bounded queries rather than one oversized regex (§3.4). |
| Bridge apply                                       | 1k upserts/frame sustainable on worker                                                                                                                                                                                                                                                                                                                                                                           |
| Pin write-back                                     | in-memory visible < 16 ms; Twin Overlay flush < 500 ms; **zero USD writes on drag**                                                                                                                                                                                                                                                                                                                              |
| Concurrent `operator` SSR                          | sustain **~100** `server_gpu` sessions per deployment without SGS collapse                                                                                                                                                                                                                                                                                                                                       |
| Logical world baseline                             | index **~10M** assets (payloads unloaded) within Phase 1 memory envelope                                                                                                                                                                                                                                                                                                                                         |
| Glass-to-glass (SSR)                               | measure baseline; set SLO after streaming phase                                                                                                                                                                                                                                                                                                                                                                  |


---

## 7. Build Phases

Phases are ordered to prove **USD scene description → interest → runtime (not USD) → bridge** before streaming or multi-protocol sprawl.

### Phase 0 — Design locks

- **Done:** scale motto = **massive world, selective high-fidelity viewers**; `viewer_profiles` per §2.5 (`operator: 100` / `engineer: 1000` / `observer: 100000`); world ≈ **10M assets** independent of GPU farm.
- Confirm latency SLO and WebRTC-only vs fallback.
- Confirm pin concurrency policy (soft lock vs LWW).
- **Done (pre-v1, v4.7):** VF USD authoring convention locked — `vf:binding:*` telemetry bindings + `customData.vf` identity/tags, assembly → group → component `payload` composition, Z-up / `metersPerUnit = 1` (reference: `[assets/usd/pump-station-01/](assets/usd/pump-station-01/)`). A codeless `VfTelemetryBindingAPI` may later formalize this as schema v1. Still confirm payload/LOD tiering per deployment.
- Confirm day-one stack includes **VictoriaMetrics** (scrape + CSV import) and that the resolver is PromQL-only for metrics; seed sample data with `[scripts/seed-victoriametrics.sh](scripts/seed-victoriametrics.sh)`.

### Phase 1 — OpenUSD index + Twin Overlay  ✅ implemented (`[sgs/](sgs/)`)

**Build:** Rust SGS skeleton that opens/indexes a USD root layer into an LSG projection (entity↔prim, extents, binding descriptors), SQLite Twin Overlay for pins/overrides, **no live values in USD**.
**Prove:** use `[assets/usd/pump-station-01/](assets/usd/pump-station-01/)` as the smoke fixture (exercises the pre-v1 `vf:` convention end-to-end: `payload`-deferred components, `extentsHint`, `customData.vf`, `vf:binding:*`), then scale to synthetic **~10M** asset refs with unloaded payloads; restart preserves pins in overlay; USD asset layers unchanged during live ops; memory stays flat without telemetry.

**Implementation choices (locked in Phase 1):**

- **USD access:** out-of-process Python `usd-core` helper (`Stage.Open … LoadNone`) → NDJSON → Rust ingest (see §3.0 "Implemented"). No OpenUSD in the Rust build.
- `**EntityId`:** stable, non-random `u64` = fixed-seed xxhash64 of the prim path (deterministic across restarts so overlay pins re-key correctly).
- **Twin Overlay:** SQLite (`rusqlite`, bundled) with a single monotonic `scene_revision`; transform pins resolve with precedence **pin > authored USD xform** (§4.1). USD files are only ever read.
- **Transform:** translate-only is captured from the bootstrap convention's xform ops; full-matrix rotation/scale is deferred (fixture and gate are translate-only).
- **CLI:** `sgs import | synth | pin | unpin | show`; overlay DB path via `--overlay` / `VF_OVERLAY_DB`.
- **Result (this hardware):** fixture → 11 entities (1 assembly / 3 groups / 7 components), 7 unloaded payload refs, 29 binding descriptors; `synth --count 10_000_000` cold-builds in ~19s (< 30s gate, §6.5) at ~5.7 GiB RSS; pins survive process restart; 12 tests green.

### Phase 2 — Interest Manager + Runtime Scene Graph  ✅ implemented (`[sgs/](sgs/)`)

**Build:** subscription API; AOI + selection triggers; activate/deactivate; USD payload load/unload; shared RSG pages; eviction; per-subscriber diff cursors.
**Prove:** moving AOI keeps `|RSG|` and open-payload count within budget; selection retains entities outside frustum; **no USD writes** from interest changes.

**Implementation choices (locked in Phase 2):**

- **Broad-phase index:** a coarse uniform grid (`[sgs/src/spatial.rs](sgs/src/spatial.rs)`) keyed by each entity's extent centroid narrows the world to a small candidate set per camera update; the Interest Manager then applies the precise predicate. Kept as **derived state** rebuilt from the LSG so the LSG stays a pure existence index.
- **Origin-agnostic subscriptions:** `[Subscription](sgs/src/interest.rs)` carries AOI (`Sphere` | `Aabb` | `Frustum`), explicit `entity_ids` (selection **bypasses** the frustum), a `tags_filter`, and a `budget` (nearest-first cap). CLI-built now; the identical struct is what the Phase 5 bridge will build from the wire — additive, not a rewrite. Profile/interaction rules are enforced (`observer` cannot request `full`).
- **RSG shared pages + grace eviction:** one `[RuntimeEntity](sgs/src/rsg.rs)` per active entity, refcounted by `SubscriptionId`; the last release schedules eviction, and only after a grace period does the page drop **and** its payload unload (one rule governs both). Each subscription drains its own `RsgDiff` (upserts/removes) — the seam Phase 5 consumes.
- **Payload hydration:** on activation the same out-of-process Python helper (`[sgs/tools/usd_export.py](sgs/tools/usd_export.py)` `--payload`) opens the component payload to surface the defaults the Phase 1 unloaded pass could not see (`kind`, `customData.vf.class`, a coarse geometry bbox + prim count), cached by `content_hash` so the three component files backing millions of instances open only three times. Surfaced metadata lives **only in the RSG**, never written back to the LSG or USD.
- **Result (this hardware):** on the `synth --count 10_000_000` world, a moving radius-15 AOI evaluates in **~0.3–1.0 ms/step** (< 2 ms p99 gate, §6.5), holds `|RSG|` bounded (~286 with a 2-tick grace) and **open payloads at 3**, evicts on schedule, and leaves the LSG revision unchanged (zero USD writes — verified byte-identical on the real `[assets/usd/pump-station-01/](assets/usd/pump-station-01/)` fixture); 27 tests green (20 unit + 7 Phase 2 integration), Phase 1's 6 still pass.

### Phase 3 — Telemetry resolvers (lazy)  ✅ implemented (`[sgs/](sgs/)`)

**Build:** resolver trait; **VictoriaMetrics** PromQL adapter (instant + batched); cache/TTL/priority; document CSV→VM import for historical IoT (resolver does not parse CSV); alert push → forced subscription; bindings read from VF USD schemas.
**Prove:** inactive bindings cause zero VM traffic; activating 1k entities batches cleanly; stale quality on VM outage; stage still free of live values; backfilled CSV series resolve via the same PromQL path as live scrapes.

**Implementation choices (locked in Phase 3):**

- **HTTP client:** blocking `ureq` (pure-Rust, no tokio) behind a `[Resolver](sgs/src/resolver.rs)` trait, so offline / VM-free tests need no network. Locked caveat: the trait insulates against swapping the HTTP client but **not** against a future sync→async transition of `resolve` itself — that is a real signature change, only warranted if concurrent resolver calls are actually needed (multi-node SGS / large fan-out).
- **Batch-by-metric + `asset` demux:** `[VictoriaMetricsResolver](sgs/src/resolver.rs)` parses each binding's `metric{asset="TAG"}` query, groups requests that share a metric (and non-`asset` matchers) into **one** `metric{asset=~"A|B|C"}` instant query (`GET /api/v1/query`, `asset` values regex-escaped), and demuxes the vector result back to entities by the `asset` label (the label the pre-v1 convention binds to `vf.assetTag`, §4.2/§4.7). Queries that do not fit the convention shape fall back to being issued individually. The resolver is the **only** component that speaks PromQL — the bridge / O3DE never do.
- **Chunk-size cap on the alternation (locked):** metric batching was baselined at camera-AOI scale (~~hundreds of active entities); a persistent automation rule such as "all pumps in alarm" (§3.2) can force-activate far more at once and would otherwise collapse into one unboundedly-large regex/URL. The planner therefore caps each `asset=~~"…" `query at **`MAX_ASSETS_PER_QUERY`(512, tunable)** and splits an over-cap group into`ceil(N / cap)` bounded queries — AOI- and moderate-automation-scale activations still collapse to one query per metric; a thousands-of-asset rule degrades gracefully into a few bounded queries instead of one giant query.
  - **Latency cost + async trigger:** chunking is not free. The resolver is still synchronous `ureq` (the Phase 3 locked sync→async caveat above), so splitting one pass into `N` queries costs `N×` wall-clock latency on the blocking client. This does **not** justify async now — but it makes the trigger condition concrete: **routine chunking under real automation-rule load is the signal** to revisit the deferred sync→async decision (concurrent resolver calls), not a speculative "we might need it eventually." Until then, one query per metric remains the common case.
- **Offline-provable batching:** the shared batch planner is used by both resolvers, and `[StubResolver](sgs/src/resolver.rs)` counts upstream round-trips the same way, so metric batching *and* "zero traffic when inactive" are provable in CI **without a VM**. To make this real at scale, the Phase 1 `[synth](sgs/src/synth.rs)` generator now authors convention-shaped queries (`metric{asset="E<i>"}`) so `--synth` worlds also batch (3 metrics → 3 upstream queries regardless of the active count).
- **RSG-resident TTL cache:** resolved values live in a per-entity `telemetry` map on the `[RuntimeEntity](sgs/src/rsg.rs)` (§4.4) — this *is* the cache (no parallel structure). `[resolve_active](sgs/src/resolver.rs)` walks **only the active working set**, skips still-fresh entries (`as_of + ttl`, wall-clock stamped), batches the rest **high-priority first**, calls the resolver once, and writes results back into the RSG only. A failed refresh keeps the prior value and downgrades it to `stale` (stale-while-revalidate); a binding that never resolved stays `unavailable`.
- **Quality + priority model:** each cached value carries one of four quality flags — `ok` | `stale` | `unavailable` | `error` (§3.4 responsibility 5). `error` is **reserved** for a bad-source / parse failure; the current VM and stub adapters map network/parse/no-data failures to `unavailable`. Priority is a two-level projection of the binding's `priority` string (`high` vs everything-else-`background`) used solely to order the batch (high first); it is not yet a separate resolver QoS lane. Per-pass observability counters (`[ResolveStats](sgs/src/resolver.rs)`: requests issued, cache hits, hit ratio, upstream delta, ok/stale/unavailable) back the §6.3 resolver metrics.
- **Alert seam:** `[sgs/src/alert.rs](sgs/src/alert.rs)` turns an `AlertEvent` (drained from an `AlertSource`) into a forced `Subscription` (kind `AlertRule`, explicit `entity_ids`, `region: None`) that **force-activates** implicated prims regardless of the AOI (§3.2). Scope choice: the alert only forces *activation* — per-binding resolver priority is still read from the binding descriptor, with **no** extra priority bump added, since alert-relevant attributes (e.g. `running`, `breakerClosed`) are already authored `priority = high` in the convention. Seam + `StubAlertSource` only this phase; a real MQTT/webhook receiver is a thin adapter behind the same seam (does not change the forced-subscription mechanism).
- **Historical CSV → VM:** unchanged from the design — imported into VictoriaMetrics (README), never parsed by the resolver, so backfilled series resolve through the **identical** `/api/v1/query` path as live scrapes.
- **Result (this hardware):** on `synth --count 100000` a moving AOI resolves the active working set with metric batching (32 active bindings → **3** upstream queries) and a rising cache-hit ratio once warm; the real `[pump-station-01](assets/usd/pump-station-01/)` fixture resolves **29 bindings via 12** metric-batched queries with **zero USD writes** and an unchanged LSG revision; an outage keeps prior values as `stale`. 6 Phase 3 integration tests + resolver/alert unit tests green (**50 tests** total across the crate). Demo: `sgs resolve <usd|--from-json|--synth N> [--offline [--stub-value V] | --outage | --vm-url URL] [--alert SEL]` — `--offline` uses a canned stub, `--outage` a down stub (exercises stale-while-revalidate), `--vm-url` a live VM.

### Phase 4 — Flow3D DSL ↔ USD/overlay  ✅ implemented (`[sgs/](sgs/)`)

**Build:** compiler emitting twin semantics into USD layers and/or Twin Overlay with stable IDs and incremental patches; bindings as bindings, not values.
**Prove:** reload patches without RSG storm; unchanged prim IDs survive; vendor geom layers remain untouched.

**Implementation choices (locked in Phase 4):**

- **DSL surface:** a small, line-oriented, hand-written language (`.flow3d`) — `scene` / `part <selector> { tag | meta | anchor … at (x,y,z) | bind <attr> metric("<promql>") [unit … ttl …ms priority …] }` / `pipe A.anchor -> B.anchor`. Selectors are the USD `vf.assetTag` (or a prim path), reusing `[Lsg::resolve_selector](sgs/src/lsg.rs)`. Chosen over YAML/TOML/JSON because the DSL is meant to be hand-authored; a structured format cannot express this shape cleanly.
- **First-class diagnostics:** every token and AST node carries a line/column `[Span](sgs/src/dsl/diag.rs)`; `[lexer](sgs/src/dsl/lexer.rs)` → `[parser](sgs/src/dsl/parser.rs)` (recursive-descent, **collects all errors** with recovery, not first-fail) → `[compile](sgs/src/dsl/compile.rs)` render rustc-style `file:line:col` messages with a caret underline. Unresolved selectors / dangling pipe anchors report at their exact source span.
- **Target = Twin Overlay + in-memory LSG patch (not USD authoring):** the Rust build has no USD dependency, so the compiler lowers to durable `[Opinion](sgs/src/opinion.rs)`s (`Binding` / `Tag` / `Meta` / `Anchor` / `Edge`) persisted in an extended SQLite Twin Overlay (`[overlay.rs](sgs/src/overlay.rs)`: `dsl_opinions` + `compile_stamps`) and applied to the LSG in place. **USD is never written** (vendor layers stay byte-identical; verified in `[tests/phase4.rs](sgs/tests/phase4.rs)`). Optional slow-path `.usda` session-layer export is deferred.
- **Stable IDs:** part → `EntityId` = hash(prim path) (inherently stable); anchor id = hash(entity + name); edge id = hash(ordered endpoints). An unchanged reload yields identical opinion keys.
- **Incremental reload = minimal patch:** each opinion carries a stable `key` (slot identity) and a `content_hash`; a recompile diffs against the stored `compile_stamps` into `{added, changed, removed, unchanged}` and `[opinion::reconcile](sgs/src/opinion.rs)` applies **only** the delta in place. Unchanged slots keep their `EntityId` and are never re-applied — so re-evaluating the **same** `InterestManager` over the patched LSG emits **zero** activate/deactivate transitions (no RSG storm), since bindings/tags do not move extents and IDs are stable.
- **Bindings stay declarative:** the DSL lowers `bind` to a `[TelemetryBinding](sgs/src/lsg.rs)` descriptor (`source_id=victoriametrics`, `query`, `unit`, `ttlMs`, `priority`, `qualityPolicy=stale_ok`) — it never carries or emits a live value; resolution remains the Phase 3 lazy path.
- **Result (this hardware):** on the `[pump-station-01](assets/usd/pump-station-01/)` fixture + `[pump-station-01.flow3d](assets/flow3d/pump-station-01.flow3d)`: 16 opinions applied across 4 entities (8 anchors, 2 pipes) with **0 USD writes**; the `--reload` demo (`[pump-station-01.reload.flow3d](assets/flow3d/pump-station-01.reload.flow3d)`) is a single changed opinion → 1 touched entity → **0/0 interest transitions** (|RSG| stable). Demo: `sgs compile <file.flow3d> <usd|--from-json|--synth N> [--reload <edited.flow3d>]`. 6 Phase 4 integration tests + DSL/opinion/overlay/resolver-cap unit tests green (**71 tests** total across the crate).

### Phase 5 — Renderer Bridge API + fake bridge  ✅ implemented ([`sgs/`](sgs/))

**Build:** `vf.bridge.v1` schema, snapshot+diff protocol, `GeomRef` carrying USD URIs, write-back pin/unpin, coarse pick.
**Prove:** fake bridge reconstructs a scene from diffs + fixture USD; reconnect resyncs; engine not required yet.

**Implementation choices (locked in Phase 5):**
- **Schema, not transport.** [`sgs/src/bridge.rs`](sgs/src/bridge.rs) defines `vf.bridge.v1` as `serde`-serializable [`BridgeMsg`](sgs/src/bridge.rs) (SGS→Bridge: `Hello` / `SnapshotBegin` / `UpsertEntity` / `RemoveEntity` / `SetTransform` / `SetGeomRef` / `SetVisualState` / `SetOverlayHint` / `SnapshotMarker` / `PinConfirm` / `PickResult`) and [`BridgeRequest`](sgs/src/bridge.rs) (Bridge→SGS: `Connect` / `UpdateAoi` / `PickRequest` / `PinPart` / `UnpinPart` / `SubscribeExtras` / `Heartbeat`) — mirroring the §3.5 op tables. Per §3.5 "the contract is the message schema + semantic versioning, not the transport", Phase 5 exchanges `Vec<BridgeMsg>` batches **in-process**; JSON round-trip is proven (wire-ready) but a real transport (WebSocket for the WebGPU/observer path per the §3.5 amendment; Unix-socket / shared-memory for co-located SSR) is deferred to Phases 6–7. Version is negotiated on connect (`negotiate`, spec §5) — this is what makes "engine not required yet" literally true.
- **Server reads three tiers, owns none.** [`BridgeServer`](sgs/src/bridge.rs) is stateless apart from a monotonic checkpoint `seq`; `snapshot()` and `encode_diff()` take `&Lsg` (defaults), `&Rsg` (active set + telemetry), and `&TwinOverlay` (pins) explicitly. The RSG diff seam ([`RsgDiff`](sgs/src/rsg.rs), reserved for this phase back in Phase 2) is drained straight into `UpsertEntity`/`RemoveEntity`; the same origin-agnostic [`Subscription`](sgs/src/interest.rs) Phase 2 built from the CLI is what a bridge reshapes from the wire (additive, not a rewrite).
- **Transform precedence = pin > authored** via [`TwinOverlay::resolved_transform`](sgs/src/overlay.rs) (spec §4.1). Resolved telemetry from the RSG is forwarded as `SetVisualState` (`value` + `quality`), but Phase 5 does **not** yet let telemetry drive the transform — the full §4.1 total order (authored < telemetry override < pin, per attribute, pending/committed pins sharing one rank) enforcement lands in Phase 8.
- **Write-back = Twin Overlay only.** `PinPart` / `UnpinPart` flow through [`BridgeServer::handle_pin`](sgs/src/bridge.rs) / `handle_unpin` to the SQLite overlay (durable, revision-bumped) and reply with `PinConfirm`; **zero USD writes** and the LSG index revision is unchanged. Because the pin lives in the overlay (not the render session), it survives a bridge reconnect.
- **Coarse pick (§1.3 non-goal).** `coarse_pick` runs a slab-method ray-vs-AABB (`ray_aabb`) over the subscription's active entities' authored extents and returns the nearest hit — portable but imprecise by design; bridges may refine with GPU picking later (§8.7).
- **Fake bridge = disposable Render Scene cache.** [`sgs/src/fake_bridge.rs`](sgs/src/fake_bridge.rs) `FakeBridge` maintains a `HashMap<EntityId, RenderEntity>` by applying `BridgeMsg`s, hydrating geometry by resolving each `GeomRef.payload_uri` against the fixture USD through the existing [`PayloadCache`](sgs/src/hydrate.rs) — proving reconstruction from **diffs + fixture USD**, never a live stage. It obeys the §3.5 hard rule (invents no IDs, persists no pins as truth). **Reconnect** = `disconnect()` drops the cache, then a fresh `Hello` + `snapshot()` rebuilds it identically (catch-up-from-`seq` cursor is a documented simplification, not built).
- **Result (this hardware):** demo `sgs bridge <usd|--from-json|--synth N> [--select …] [--pin SEL --pin-translate x,y,z]` reconstructs the AOI's active set into the Render Scene from the diff stream, hydrates each distinct payload once, coarse-picks the nearest entity, writes a pin back through the overlay (reflected in the next upsert, revision bumped), and after a disconnect **rebuilds the scene identically from a snapshot** with the pin intact — `USD writes: 0`, LSG revision unchanged. 7 Phase 5 integration tests + bridge/fake-bridge unit tests green (**85 tests** total across the crate).

### Phase 5.5 — WebGPU Observer Client (WebSocket bridge)  ✅ implemented ([`sgs/`](sgs/) + [`web/`](web/))

**Build:** realize the Phase 5 `FakeBridge` seams over a **real wire** for the `observer` profile (spec §2.5 / §3.6 WebGPU tier): a blocking `sgs serve` WebSocket server and a browser WebGPU client that reconstructs the active set as **AABB proxy boxes** from `vf.bridge.v1` diffs and sends camera/pick/pin back. This is the "rendering today" path — no engine required.
**Prove:** open the page and see proxy boxes stream in from a live WebSocket diff feed; moving the camera AOI changes the active set; clicking coarse-picks an entity; pinning moves a box and persists to the Twin Overlay; reloading the page reconnects and rebuilds the scene identically.

**Implementation choices (locked in Phase 5.5):**
- **Transport = WebSocket, Rust stays sync / no-tokio.** The §3.5 2026-07 Transport amendment is realized with blocking [`tungstenite`](https://crates.io/crates/tungstenite) over [`std::net::TcpListener`](sgs/src/serve.rs), **thread-per-connection**, matching the crate's `ureq` (blocking) ethos — no async runtime is introduced. WebRTC/Wilbur stay reserved for the SSR **video** path (§3.7) and are untouched. `sgs serve` negotiates + `Hello`s on connect, sends a snapshot, then maps each inbound [`BridgeRequest`](sgs/src/bridge.rs) → the existing [`BridgeServer`](sgs/src/bridge.rs) method (`snapshot` / `encode_diff` / `coarse_pick` / `handle_pin` / `handle_unpin`) and pushes `BridgeMsg` JSON-array frames down. **One [`Subscription`](sgs/src/interest.rs) per connection**; the LSG + spatial index are shared read-only (`Arc`) and the Twin Overlay behind a mutex (the one mutation any connection performs is a pin write-back). It is the scripted [`cmd_bridge`](sgs/src/main.rs) demo reshaped from a fixed AOI walk into an inbound-request-driven loop — same seams, real wire.
- **Fidelity = AABB proxy boxes tinted by telemetry quality.** The browser renders each active entity as an instanced box (WebGPU), sized from authored `extents` and tinted `ok` / `stale` / `unavailable` from the RSG telemetry the server forwards as `SetVisualState`/`UpsertEntity.visual`. **No in-browser USD mesh decode** (that is separate later work). The client's [`RenderScene`](web/src/renderScene.ts) is a direct **port of [`FakeBridge::apply`](sgs/src/fake_bridge.rs)** and obeys the §3.5 hard rule (invents no ids, persists no pins as truth; a reconnect resyncs from a fresh snapshot).
- **`extents` on the wire = additive, still `vf.bridge.v1`.** [`BridgeMsg::UpsertEntity`](sgs/src/bridge.rs) gains an additive `Option<Aabb>` `extents` field (`#[serde(default, skip_serializing_if=…)]`) — **no v2 bump**. It is sent **relative to the authored transform origin** so a pin that moves `transform` moves the box, while an unpinned entity renders at its original world AABB.
- **`EntityId` wire encoding = hex string.** A full 64-bit id exceeds JavaScript's safe-integer range (2^53), so a bare JSON number would be silently corrupted by the browser's `JSON.parse` and break pick/pin id round-trips. `EntityId` therefore (de)serializes as its 16-char hex string (matching `as_hex()` / the overlay's `entity_id TEXT`) — lossless across languages, still `vf.bridge.v1`.
- **Result (this hardware):** `sgs serve <usd|--from-json|--synth N> [--vm | --outage]` streams the active set to the observer; a Rust loopback WS test and a headless Node `WebSocket` client both assert wire reconstruction == snapshot, a moving AOI streams a diff, a pin survives a reconnect via the overlay, and the LSG revision is unchanged (`USD writes: 0`). **89 tests** total across the crate (was 85: +1 Phase 5 extents round-trip, +3 Phase 5.5 loopback).

### Phase 5.6 — VF Geometry Store + WebGPU mesh hydration (glTF/GLB)  ✅ implemented ([`sgs/`](sgs/) + [`web/`](web/))

**Build:** realize the *"(future) VF geometry store"* named on [`GeomRef`](sgs/src/lsg.rs) (§3.1 / §4.2) and turn the observer's proxy boxes into real USD geometry. Extract triangulated meshes from USD payloads **at import** into a **content-addressed store**, deliver each mesh to the observer by its `GeomRef.content_hash`, and have the WebGPU client hydrate and draw the real mesh — **keeping the AABB proxy box as the LOD-0 fallback** so an absent / in-flight mesh never regresses Phase 5.5. Phase 6 (O3DE) reuses the same store, so it is built here as a shared, renderer-independent foundation.

**Prove:** `sgs serve assets/usd/pump-station-01` streams the active set and the observer renders real pump/tank/switch geometry fetched by `content_hash`, falling back to boxes when a mesh is absent or in flight; the store tessellates each unique payload **once** (reconnect re-fetches with no redundant work); `USD writes: 0` and OpenUSD stays off the Rust build and runtime hot path.

**Implementation choices (locked for Phase 5.6):**
- **Open standard = glTF 2.0 / GLB (Khronos).** The geometry store and the wire format are GLB — engine-neutral so O3DE/Unreal reuse it, and trivially loadable in the browser. **No bespoke binary format** and **no heavyweight glTF *engine* dependency**: a small focused GLB reader/writer in the crate's minimalist style. Licenses stay **MIT / Apache-2.0 / BSD** (no GPL/AGPL linked into SGS, no EULA/royalty tools on the default path), consistent with §3.7.
- **OpenUSD stays import / asset-prep only.** Mesh tessellation runs out-of-process in [`usd_export.py`](sgs/tools/usd_export.py) (a new `--mesh <prim>` mode), never on the Rust build or the runtime hot path — the same rule the current import / hydration obey.
- **Store = read-only, content-addressed, refcounted.** Keyed by `GeomRef.content_hash` (identical payloads tessellate/store **once**), mirroring the payload cache in [`hydrate.rs`](sgs/src/hydrate.rs). Derived and disposable; never written back to USD, the LSG, or the Twin Overlay (`USD writes: 0`). Synthetic / NDJSON worlds have no on-disk mesh → proxy box.
- **Transport = additive, one connection, still `vf.bridge.v1`.** A new `BridgeRequest::FetchGeom { content_hash }` is answered with the **GLB bytes as a binary WebSocket frame** over the existing blocking [`tungstenite`](sgs/src/serve.rs) connection — no async runtime, no second server / port, no v2 bump. Unknown hash is handled gracefully (client keeps the box).
- **Fidelity = proxy box is LOD-0.** An entity with no `GeomRef`, an unresolved hash, or a not-yet-fetched mesh renders exactly as in Phase 5.5; the real mesh layers in on top when it arrives, preserving telemetry tint, selection highlight, and edge outline.
- **Result (this hardware):** [`usd_export.py --mesh`](sgs/tools/usd_export.py) deterministically tessellates the fixture gprims (Cube/Cylinder) out-of-process; the content-addressed [`GeomStore`](sgs/src/geomstore.rs) (built from the LSG, process-shared behind a `Mutex`) encodes each unique payload to GLB with a small in-crate writer/reader (no glTF engine dep) and tessellates it **once**. `sgs serve assets/usd/pump-station-01` streams the active set and the observer ([`web/src/renderer.ts`](web/src/renderer.ts)) fetches meshes by `content_hash` over an additive [`FetchGeom`](sgs/src/bridge.rs) → **binary GLB frame** on the same `tungstenite` connection (no v2 bump, no second port), drawing real pump/tank/switch geometry with the telemetry tint + selection highlight + edge outline and falling back to the proxy box when a mesh is absent or in flight. The three component files dedup to **3 store entries** for 7 instances; a reconnect re-fetches with no redundant tessellation; `USD writes: 0`, LSG revision unchanged, OpenUSD absent from the Rust build and runtime hot path. A Python-free loopback WS test (known hash → valid GLB frame, unknown → empty frame), a USD-gated store test (3 deduplicated non-empty meshes, tessellate-once, zero LSG mutation), and a toolchain-gated headless Node mesh test (fetch a GLB by hash → positive vertex count; geom-less entities still reconstruct) all pass. **94 tests** total across the crate (was 89: +3 geomstore unit, +2 Phase 5.6 integration).

### Phase 6 — O3DE Bridge (Render Scene cache)

**Build:** O3DE Gem applying bridge diffs; hydrate geom via USD where practical; expose render target as a `**FrameSource`** (the seam the reference streamer already defines).
**Prove:** renderer remains disposable — kill Gem, restart, state returns from SGS; live visuals do not require stage mutation.

### Phase 7 — Streaming (Wilbur + O3DE Streamer)

**Build:** stand up Wilbur (formerly Cirrus) + SFU from Epic's images and land the permanent **reference streamer + CI harness** first (synthetic `FrameSource`, WebRTC to Wilbur, drop/reorder/latency gate); then the Streamer Gem (capture/encode/WebRTC) speaking Wilbur, swapping the O3DE `FrameSource` in behind the same encode/transport/signalling path; DataChannel control messages.
**Prove:** harness is green on the synthetic path (plumbing), then measure the real O3DE-sourced glass-to-glass baseline separately (encoder under real content); decide WebRTC fallback explicitly.

### Phase 8 — Interaction polish

**Build:** pick path, pin precedence — enforce the §4.1 total order **authored USD default < telemetry-driven override < pin (pending or committed)** (per attribute; pending drag pins and committed pins share one rank so a live tick cannot fight an in-flight drag) — overlay hints, client drag prediction.
**Prove:** two viewers; one pins; other sees authoritative update via SGS; USD files unchanged until optional export.

### Phase 9 — Orchestrator + multi-session

**Build:** worker pool sized for `**operator.count_target: 100`**, sticky routing, crash replace, authn/authz + **profile enforcement** at edge; shared/follow views for engineers/observers without extra GPU.
**Prove:** sustain ~100 `server_gpu` sessions; worker death loses no pins; `observer` / API traffic does not allocate SSR workers; world size growth does not force pool growth.

### Phase 10 — Hardening

**Build:** metrics/tracing, load suite against §6.5 gates, optional second resolver, optional Unreal/WebGPU bridge spike, optional USD override-layer export for DCC.
**Prove:** published benchmark report; known failure drills documented; confirm zero hot-path USD write traffic under load.

**Removed vs v3:** “Data Integration Layer as a push bus for all protocols” is no longer an early phase. **Rejected vs Omniverse-default:** live Nucleus/stage as telemetry fabric.

---

## 8. Risks & Tradeoffs

1. **World scale ≠ render scale.** Interest management reduces *scene* work; encode cost follows `**operator` count only**. Growing to 10M→100M assets must not imply growing the GPU farm. Growing observers must not either.
2. **O3DE streaming is still custom engineering.** Reusing Wilbur reduces signaling work; capture/encode/WebRTC Streamer remains non-trivial and under-precedented in O3DE. The reference streamer de-risks signalling/transport early, but the real encode path behind the `FrameSource` seam is still the hard, unproven part.
3. **Lazy telemetry adds complexity vs “just subscribe to everything.”** Correct for scale; requires good caching and stale UX. Poor TTL/priority tuning will look like “broken live data.”
4. **Three-tier state (USD/LSG · RSG · Render) can be over-abstracted.** Implement as clear modules inside one SGS binary first — not three networked services. USD is files + an index, not a microservice by default.
5. **OpenUSD in Rust is non-trivial.** Expect FFI to OpenUSD C++ or a small helper process; budget integration risk early (Phase 1), don’t pretend it’s a pure-Rust crate day one.
6. **Temptation to “just write live attrs into USD.”** Short-term demo win; long-term scale and multi-viewer failure. Gate this in code review / benchmarks (zero hot-path USD writes).
7. **Coarse SGS picking vs GPU picking:** agnostic pick is portable but imprecise; document bridge-side refinement as optional.
8. **SQLite Twin Overlay is right until it isn’t.** Fine for single-node overrides; wrong if you prematurely shard without a consistency story. USD assets already scale as object storage.
9. **Wilbur coupling:** faster path to WebRTC; accept Epic signaling protocol gravity (and per-UE-line protocol drift) or budget a thin adapter. The streamer/harness isolate WebRTC + signalling specifics so a version bump is contained.
10. **Multi-user edits:** without locks, pin fights confuse operators; with locks, automation must participate. Do not overload USD layer opinions as the live lock manager.
11. **Omniverse gravity:** teams familiar with Nucleus may push to make USD the runtime fabric. Reassert the split: USD for description/geometry; SGS for runtime.
12. **Profile leakage:** product pressure may try to put `observer` or all `engineer` sessions on `server_gpu`. Resist unless GPU budget explicitly expands; enforce `viewer_profiles` at the Orchestrator.

---

## 9. Over-engineered vs Under-specified (audit)

### 9.1 Over-engineered / premature


| Item                                                                         | Why push back                                                                                                                |
| ---------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| Always-on multi-protocol Data Integration event bus                          | Pushes the entire telemetry universe toward SGS; invert to subscription-driven resolve                                       |
| Kafka/NATS “when we scale” as ambient architecture                           | Add when fan-out/durability is measured — not as default topology                                                            |
| Treating SGS as live value store                                             | Duplicates historian/TSDB; destroys the scale envelope                                                                       |
| Using OpenUSD / Nucleus as live telemetry + multi-viewer state fabric        | Correct for DCC co-edit demos; wrong for sensor-rate digital twins                                                           |
| Building WS video fallback + full streaming + all connectors in early phases | Sequencing should prove interest + bridge before protocol sprawl                                                             |
| Multiple persistence systems for the same facts                              | USD for assets; SQLite for twin overlay; **VictoriaMetrics for time series** — don’t also mirror metrics into Postgres/Redis |
| CSV parsers inside the hot-path resolver                                     | Import historical IoT CSV into VictoriaMetrics; resolver stays PromQL-only                                                   |


### 9.2 Under-specified earlier (addressed in v4.1)


| Gap                                 | Answer                                                        |
| ----------------------------------- | ------------------------------------------------------------- |
| Geometry interchange format unnamed | OpenUSD as scene-description + geometry layer                 |
| Runtime vs authored state blurred   | USD/LSG vs RSG ownership split; zero hot-path USD writes      |
| No Runtime Scene Graph              | Explicit RSG working set with eviction                        |
| AOI-only interest                   | Subscription model with multiple triggers; payload-aware      |
| Telemetry at millions of entities   | Bindings on prims + lazy resolvers + cache/TTL/batch/priority |
| Multi-user concurrency              | Revisioned Twin Overlay + soft lock / LWW policy              |
| Security                            | Authz inside Interest Manager; secret isolation in resolvers  |
| Observability / testing / benches   | §6.3–6.5                                                      |
| API versioning                      | `vf.bridge.vN` + USD revision + overlay revision              |
| Failure recovery                    | Matrix in §6.1                                                |
| Cardinality targets                 | Logical / runtime / render envelope in §2.4                   |


### 9.3 Assumptions still challenged (open product decisions)

- ~~Is server-side O3DE for every viewer still correct?~~ **Resolved:** only `operator` (and scarce `engineer` SSR grants); motto is massive world / selective high-fidelity viewers (§2.5).
- Do AI agents share the same Interest Manager as human viewers? (**Yes — recommended**; budget against RSG / profile, not automatically against SSR GPU.)
- Should alert systems write into USD or only poke subscriptions? (**Subscriptions + optional annotation prims on slow path**; don’t turn alerts into live USD spam.)
- How much geometry decode happens in SGS vs native USD loaders in the bridge? (**Prefer bridge/native hydration when the engine supports it; SGS keeps bounds + refs.**)
- Is Omniverse Nucleus ever in-scope? (**Only as an optional DCC collaboration frontend to authored layers — never as the telemetry runtime.**)
- When do `engineer` hybrid and `observer` paths ship relative to `operator` SSR? (**Recommend: profile routing + read-only bridge/query stub soon after Phase 5–6 so non-operators are never forced onto GPU workers.**)

---

## 10. Summary Verdict

v3 made the Scene Graph Service authoritative and the renderer swappable. v4 stopped treating the scene graph as a live telemetry database. v4.1 completed the content story with OpenUSD. v4.2 introduced viewer tiers. **v4.3 states the scale thesis cleanly:**

> **Massive world, selective high-fidelity viewers.**  
> ~10M assets can exist; ~100 operators consume GPU; ~10k observers and ~100k API users ride lighter paths. **The world scales independently from render sessions.**

1. **OpenUSD** = **initialization / import format** (geometry, materials, hierarchy, metadata defaults + declarative bindings) — **not a runtime dependency**
2. **LSG** = index over USD-seeded defaults + Twin Overlay (self-sufficient after import)
3. **Interest / Subscriptions** = activation (payload-aware)
4. **RSG** = hot working set + lazy telemetry — **runtime state engine** that owns the imported geometry/material/hierarchy/metadata **defaults** and may **scale/transform** them from telemetry (and other inputs), **renderer-independently** (queries **VictoriaMetrics**, does not own the historian)
5. **Render Scene** = disposable GPU cache behind `vf.bridge.v1`
6. `**viewer_profiles`** = `operator` (server_gpu, full) / `engineer` (hybrid, limited) / `observer` (WebGPU|video|dashboard, read_only)

**Hard rule:** OpenUSD is an **initialization format, not the runtime**; once imported, the VectorFlow Scene Graph owns and may scale/transform geometry, materials, hierarchy, and metadata defaults from telemetry, renderer-independently, with no live USD dependency.  
**Cost rule:** Do not optimize to render a billion users. Optimize so a billion objects can exist while only full-simulation users consume GPU — same as games, GIS, and industrial platforms.  
**Telemetry rule:** VictoriaMetrics holds series (live scrape + CSV/IoT import); the resolver is a thin PromQL client over active subscriptions only.
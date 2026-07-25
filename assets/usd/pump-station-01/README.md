# Pump Station 01 — OpenUSD bootstrap scene

First sample OpenUSD scene for VectorFlow3D. It seeds a small industrial
facility — **pumps, storage tanks, and electrical distribution switches** — that
the Scene Graph Service imports at initialization to build the Logical Scene
Graph (LSG). It is an **initialization / import artifact, not a runtime
dependency** (see [`vectorflow3d-spec-scenegraph.md`](../../../vectorflow3d-spec-scenegraph.md) §3.0).

## Layout

```
assets/usd/pump-station-01/
├── pump_station.usda              # top-level facility ASSEMBLY (open this)
└── components/
    ├── pump.usda                  # reusable Pump component
    ├── tank.usda                  # reusable Tank component
    └── distribution_switch.usda   # reusable switchgear component
```

`pump_station.usda` is the bootstrap layout / root layer (`defaultPrim =
PumpStation01`). Component geometry is deferred behind **`payload`** arcs, which
are the interest / streaming quantum in the spec (§3.2, Phase 2). The assembly
instances 3 pumps, 2 tanks, and 2 distribution switches.

Stage conventions: **Z-up**, `metersPerUnit = 1`. Geometry is inline USD gprims
(cylinders / cubes) so the sample is self-contained — no external mesh assets.

## Model hierarchy (USD kinds)

```
PumpStation01            assembly
├── PumpHall             group
│   ├── Pump_01..03      component   (payload -> components/pump.usda)
├── TankFarm             group
│   ├── Tank_A / Tank_B  component   (payload -> components/tank.usda)
└── Distribution         group
    └── SWG_01 / SWG_02  component   (payload -> components/distribution_switch.usda)
```

This assembly → group → component chain maps directly onto the LSG
`EntityId ↔ prim path` index.

## What lives where (spec §4.1 ownership split)

- **Component files** carry geometry, materials, `kind = component`,
  `assetInfo`, and static `customData.vf` **defaults** (class, manufacturer,
  ratings).
- **The assembly** carries, per instance and *without needing to load payloads*:
  transform, a coarse `extentsHint`, `customData.vf` identity/tags, and the
  declarative telemetry **bindings**.
- **No live telemetry values are authored anywhere** — USD holds defaults and
  bindings only (spec hard rule). Live values resolve into the Runtime Scene
  Graph at subscription time.

## VectorFlow authoring convention (`vf:`) — pre-v1

The VF USD schema is not locked yet (spec Phase 0). Until a codeless
`VfTelemetryBindingAPI` is defined, this scene establishes a forward-compatible
convention:

### Identity, tags, static metadata → `customData.vf`

Dictionary-valued metadata composes key-by-key, so a component's defaults merge
with per-instance opinions:

```usda
customData = {
    dictionary vf = {
        string class = "pump"          # from components/pump.usda (default)
        string assetTag = "PUMP-01"    # from the instance (override)
        string[] tags = ["rotating_equipment", "zone:pump_hall", "duty"]
    }
}
```

### Telemetry bindings → `vf:binding:<attribute>:*` attributes

Each binding is a set of namespaced `custom` attributes describing **how to
resolve** a value (never the value itself), matching the spec "Binding shape"
(`resolver: { source_id, query }`, §3.4 line ~372) and the `TelemetryBinding`
type (§4.2):

```usda
custom string vf:binding:flow:sourceId      = "victoriametrics"
custom string vf:binding:flow:query         = "pump_flow_gpm{asset=\"PUMP-01\"}"
custom token  vf:binding:flow:attribute     = "flow"
custom token  vf:binding:flow:unit          = "gpm"
custom double vf:binding:flow:ttlMs         = 5000
custom token  vf:binding:flow:priority      = "background"   # background | high
custom token  vf:binding:flow:qualityPolicy = "stale_ok"
```

Queries are PromQL against VictoriaMetrics (`/api/v1/query`), fully resolved per
instance (the `asset` label carries the `assetTag`). Bindings are declarative:
the Telemetry Resolver fetches them lazily only for active subscriptions.

### Bound metrics per asset class

| Class                 | Bound attributes (`vf:binding:*`)                                   |
|-----------------------|---------------------------------------------------------------------|
| `pump`                | `flow`, `dischargePressure`, `motorTemp`, `running`, `vibration`    |
| `tank`                | `level`, `volume`, `temp`                                           |
| `distribution_switch` | `loadCurrent`, `busVoltage`, `breakerClosed`, `switchTemp`          |

These metric names are the contract for a scrape/import into VictoriaMetrics;
they are not defined by this scene. To populate VictoriaMetrics with matching
sample data (same metric names + `asset` labels), run
[`scripts/seed-victoriametrics.sh`](../../../scripts/seed-victoriametrics.sh)
(see the root README "Seed sample telemetry" section).

## Validate

Uses the system Apple USD Tools (`/usr/bin/usdchecker`, `/usr/bin/usdcat`):

```bash
usdchecker assets/usd/pump-station-01/pump_station.usda      # composes + loads payloads
usdcat --flatten assets/usd/pump-station-01/pump_station.usda   # inspect composed stage
```

Both should report success / a resolved prim tree.

## Not in scope

- No `VfTelemetryBindingAPI` schema plugin (documented convention only).
- No importer / SGS code (spec Phase 1).
- O3DE never reads this directly for live state — the Renderer Bridge receives
  diffs from SGS; USD is import-time only.

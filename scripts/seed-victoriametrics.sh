#!/usr/bin/env bash
# Seed VictoriaMetrics with sample VectorFlow3D telemetry.
#
# Pushes realistic synthetic series for the assets modeled in
# assets/usd/pump-station-01 (pumps, tanks, distribution switches) using the
# SAME metric names + `asset` labels the scene's vf:binding:* queries expect,
# e.g. pump_flow_gpm{asset="PUMP-01"}. Values are written via VM's Prometheus
# text import endpoint (/api/v1/import/prometheus).
#
# Domain boundary: this is the Telemetry Resolver's PromQL surface. O3DE never
# queries VictoriaMetrics.
#
#   ./scripts/seed-victoriametrics.sh                 # one-shot, last 60 min @ 30s
#   ./scripts/seed-victoriametrics.sh --live          # backfill, then append live
#   ./scripts/seed-victoriametrics.sh --pumps 500 --switches 500 --window-min 10
#   VF_VM_URL=http://127.0.0.1:8428 ./scripts/seed-victoriametrics.sh
#
# Flags:
#   --window-min N   history window in minutes ending now   (default 60)
#   --step SECONDS   sample interval                         (default 30)
#   --pumps N        pump assets    PUMP-01..              (default 3)
#   --tanks N        tank assets    TANK-A..               (default 2)
#   --switches N     switch assets  SWG-01..               (default 2)
#   --live           after backfill, append one sample per series every --step
#   --url URL        VictoriaMetrics base URL (or VF_VM_URL env)
#   -h, --help       show this help

set -euo pipefail

VF_VM_URL="${VF_VM_URL:-http://127.0.0.1:8428}"
WINDOW_MIN=60
STEP=30
PUMPS=3
TANKS=2
SWITCHES=2
LIVE=0

usage() { sed -n '2,29p' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --window-min) WINDOW_MIN="$2"; shift 2 ;;
    --step)       STEP="$2"; shift 2 ;;
    --pumps)      PUMPS="$2"; shift 2 ;;
    --tanks)      TANKS="$2"; shift 2 ;;
    --switches)   SWITCHES="$2"; shift 2 ;;
    --live)       LIVE=1; shift ;;
    --url)        VF_VM_URL="$2"; shift 2 ;;
    -h|--help)    usage; exit 0 ;;
    *) echo "error: unknown argument '$1' (see --help)" >&2; exit 2 ;;
  esac
done

for n in "$WINDOW_MIN" "$STEP" "$PUMPS" "$TANKS" "$SWITCHES"; do
  if ! [[ "$n" =~ ^[0-9]+$ ]]; then
    echo "error: numeric flags must be non-negative integers (got '$n')" >&2
    exit 2
  fi
done
if [[ "$STEP" -lt 1 ]]; then echo "error: --step must be >= 1" >&2; exit 2; fi

IMPORT_URL="${VF_VM_URL%/}/api/v1/import/prometheus"

# Value model shared by the historical backfill and the live loop.
# Emits Prometheus text lines: name{asset="..."} value ts_ms
# Args (via -v): now_ms, step_s, npoints, pumps, tanks, switches
read -r -d '' AWK_PROG <<'AWK' || true
function noise(amp)      { return (rand() * 2 - 1) * amp }
function clamplo(v, lo)  { return v < lo ? lo : v }
function clamp(v, lo, hi){ if (v < lo) return lo; if (v > hi) return hi; return v }
function tankname(n,   s, r) {
  s = ""
  while (n > 0) { r = (n - 1) % 26; s = sprintf("%c", 65 + r) s; n = int((n - 1) / 26) }
  return "TANK-" s
}
BEGIN {
  srand()
  start_ms = now_ms - (npoints - 1) * step_s * 1000
  twopi = 6.2831853

  # --- pumps ---
  for (i = 1; i <= pumps; i++) {
    tag = sprintf("PUMP-%02d", i)
    standby = (pumps > 1 && i == pumps) ? 1 : 0
    off = i * 0.7
    for (k = 0; k < npoints; k++) {
      ts = start_ms + k * step_s * 1000
      ph = twopi * (k / (npoints > 1 ? npoints : 1))
      if (standby) {
        flow = clamplo(noise(0.5), 0); press = clamplo(1 + noise(1), 0)
        mtemp = 25 + noise(1.5); run = 0; vib = clamplo(0.1 + noise(0.05), 0)
      } else {
        flow = clamplo(300 + 15 * sin(ph + off) + noise(8), 0)
        press = clamplo(120 + 5 * sin(ph + off) + noise(4), 0)
        mtemp = 65 + 5 * sin(ph + off) + noise(3)
        run = 1
        vib = clamplo(2.0 + noise(0.6), 0)
      }
      printf "pump_flow_gpm{asset=\"%s\"} %.2f %d\n", tag, flow, ts
      printf "pump_discharge_pressure_psi{asset=\"%s\"} %.2f %d\n", tag, press, ts
      printf "pump_motor_temp_celsius{asset=\"%s\"} %.2f %d\n", tag, mtemp, ts
      printf "pump_running_state{asset=\"%s\"} %d %d\n", tag, run, ts
      printf "pump_vibration_mm_s{asset=\"%s\"} %.3f %d\n", tag, vib, ts
    }
  }

  # --- tanks ---
  for (i = 1; i <= tanks; i++) {
    tag = tankname(i)
    base = 55 + ((i * 7) % 25)   # per-tank nominal level 55..79
    off = i * 1.1
    cap = 5000
    for (k = 0; k < npoints; k++) {
      ts = start_ms + k * step_s * 1000
      ph = twopi * (k / (npoints > 1 ? npoints : 1))
      level = clamp(base + 10 * sin(ph + off) + noise(1.5), 0, 100)
      vol = level / 100.0 * cap
      ttemp = 18 + 2 * sin(ph + off) + noise(0.8)
      printf "tank_level_pct{asset=\"%s\"} %.2f %d\n", tag, level, ts
      printf "tank_volume_gal{asset=\"%s\"} %.1f %d\n", tag, vol, ts
      printf "tank_temp_celsius{asset=\"%s\"} %.2f %d\n", tag, ttemp, ts
    }
  }

  # --- distribution switches ---
  for (i = 1; i <= switches; i++) {
    tag = sprintf("SWG-%02d", i)
    off = i * 0.5
    for (k = 0; k < npoints; k++) {
      ts = start_ms + k * step_s * 1000
      ph = twopi * (k / (npoints > 1 ? npoints : 1))
      amps = clamplo(400 + 40 * sin(ph + off) + noise(15), 0)
      volts = 480 + 3 * sin(ph + off) + noise(2)
      stemp = 40 + 5 * sin(ph + off) + noise(2)
      printf "switch_load_amps{asset=\"%s\"} %.2f %d\n", tag, amps, ts
      printf "switch_bus_voltage_volts{asset=\"%s\"} %.2f %d\n", tag, volts, ts
      printf "switch_breaker_closed_state{asset=\"%s\"} %d %d\n", tag, 1, ts
      printf "switch_temp_celsius{asset=\"%s\"} %.2f %d\n", tag, stemp, ts
    }
  }
}
AWK

gen_payload() { # args: now_ms npoints
  awk -v now_ms="$1" -v step_s="$STEP" -v npoints="$2" \
      -v pumps="$PUMPS" -v tanks="$TANKS" -v switches="$SWITCHES" "$AWK_PROG"
}

push() { # stdin: prometheus text; verifies 2xx
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' --data-binary @- "$IMPORT_URL")"
  if [[ ! "$code" =~ ^2 ]]; then
    echo "error: import POST to $IMPORT_URL returned HTTP $code" >&2
    return 1
  fi
}

NPOINTS=$(( WINDOW_MIN * 60 / STEP + 1 ))
SERIES=$(( PUMPS * 5 + TANKS * 3 + SWITCHES * 4 ))
SAMPLES=$(( SERIES * NPOINTS ))

echo "==> Seeding VictoriaMetrics at $VF_VM_URL"
echo "    assets: ${PUMPS} pumps, ${TANKS} tanks, ${SWITCHES} switches  (${SERIES} series)"
echo "    window: ${WINDOW_MIN} min @ ${STEP}s  ->  ${NPOINTS} points/series, ${SAMPLES} samples"

# Fail fast if VM is unreachable.
if ! curl -sf --max-time 5 "${VF_VM_URL%/}/health" >/dev/null; then
  echo "error: VictoriaMetrics not reachable at $VF_VM_URL (start it: docker compose -f infra/victoriametrics/docker-compose.yml up -d)" >&2
  exit 1
fi

START_S=$(date +%s)
NOW_MS=$(( START_S * 1000 ))
gen_payload "$NOW_MS" "$NPOINTS" | push
ELAPSED=$(( $(date +%s) - START_S ))
[[ "$ELAPSED" -lt 1 ]] && ELAPSED=1
echo "==> Backfill done: ${SAMPLES} samples in ~${ELAPSED}s (~$(( SAMPLES / ELAPSED )) samples/s)"
echo "    verify: curl '${VF_VM_URL%/}/api/v1/query?query=pump_flow_gpm' | jq"
echo "    vmui:   ${VF_VM_URL%/}/vmui"

if [[ "$LIVE" -eq 1 ]]; then
  echo "==> Live mode: appending ${SERIES} samples every ${STEP}s (Ctrl-C to stop)…"
  trap 'echo; echo "==> stopped live seeding"; exit 0' INT
  while true; do
    sleep "$STEP"
    gen_payload "$(( $(date +%s) * 1000 ))" 1 | push \
      && printf '    +%d samples @ %s\n' "$SERIES" "$(date +%H:%M:%S)"
  done
fi

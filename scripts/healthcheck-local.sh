#!/usr/bin/env bash
# Local readiness check: VictoriaMetrics + O3DE render client (+ optional Pixel Streaming).
# Exit 0 only if metrics are healthy AND O3DE is at least built (or Docker client up).
# Use --strict to also require O3DE process/container running.
# Use --pixelstreaming (or VF_CHECK_PIXELSTREAMING=1) to also check the Pixel
# Streaming stack (Wilbur signalling + reference streamer); with --strict this
# runs the reference-streamer harness single-shot (drop/reorder/latency gate).
#
#   ./scripts/healthcheck-local.sh
#   ./scripts/healthcheck-local.sh --strict
#   ./scripts/healthcheck-local.sh --pixelstreaming
#   ./scripts/healthcheck-local.sh --pixelstreaming --strict
#   VF_VM_URL=http://127.0.0.1:8428 O3DE_ROOT=~/O3DE/o3de ./scripts/healthcheck-local.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

STRICT=0
CHECK_PS="${VF_CHECK_PIXELSTREAMING:-0}"
for arg in "$@"; do
  case "$arg" in
    --strict) STRICT=1 ;;
    --pixelstreaming|--pixel-streaming) CHECK_PS=1 ;;
    -h|--help)
      sed -n '2,13p' "$0"
      exit 0
      ;;
  esac
done

VF_VM_URL="${VF_VM_URL:-http://127.0.0.1:8428}"
O3DE_ROOT="${O3DE_ROOT:-$HOME/O3DE/o3de}"
EDITOR_APP="${EDITOR_APP:-$O3DE_ROOT/build/mac_xcode/bin/profile/Editor.app}"
EDITOR_BIN="${EDITOR_BIN:-$EDITOR_APP/Contents/MacOS/Editor}"
O3DE_DOCKER_NAME="${O3DE_DOCKER_NAME:-vf-o3de-client}"

# Pixel Streaming (opt-in via --pixelstreaming / VF_CHECK_PIXELSTREAMING=1)
PS_HTTP_PORT="${PS_HTTP_PORT:-80}"
PS_HTTP_URL="${PS_HTTP_URL:-http://127.0.0.1:$PS_HTTP_PORT}"
PS_STREAMER_PORT="${PS_STREAMER_PORT:-8888}"
PS_SFU_PORT="${PS_SFU_PORT:-8889}"
PS_SIGNALLING_NAME="${PS_SIGNALLING_NAME:-vf-pixelstreaming-signalling}"
PS_STREAMER_NAME="${PS_STREAMER_NAME:-vf-pixelstreaming-streamer}"
PS_SFU_NAME="${PS_SFU_NAME:-vf-pixelstreaming-sfu}"
PS_COMPOSE="${PS_COMPOSE:-$REPO_ROOT/infra/pixelstreaming/docker-compose.yml}"

ok=0
warn=0
fail=0

pass() { printf "  OK   %s\n" "$1"; ok=$((ok + 1)); }
warn() { printf "  WARN %s\n" "$1"; warn=$((warn + 1)); }
fail() { printf "  FAIL %s\n" "$1"; fail=$((fail + 1)); }

# TCP port reachability using bash /dev/tcp (no nc dependency).
port_open() {
  (exec 3<>"/dev/tcp/$1/$2") >/dev/null 2>&1 && exec 3>&- 3<&-
}

echo "== VectorFlow3D local health =="
echo

# --- VictoriaMetrics ---
echo "VictoriaMetrics ($VF_VM_URL)"
if curl -sf --max-time 3 "$VF_VM_URL/health" >/dev/null; then
  pass "HTTP /health"
else
  fail "HTTP /health (is compose up? docker compose -f infra/victoriametrics/docker-compose.yml up -d)"
fi

if curl -sf --max-time 3 "$VF_VM_URL/api/v1/query?query=up" \
  | grep -q '"status":"success"'; then
  pass "PromQL query up"
else
  # health can be up before first scrape; soft warn
  if curl -sf --max-time 3 "$VF_VM_URL/health" >/dev/null; then
    warn "PromQL query up (no success payload yet — scrapes may still be warming)"
  else
    fail "PromQL query up"
  fi
fi

if command -v docker >/dev/null 2>&1; then
  if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx 'vf-victoriametrics'; then
    pass "container vf-victoriametrics running"
  else
    warn "container vf-victoriametrics not listed (VM may be remote or named differently)"
  fi
fi

echo

# --- O3DE ---
echo "O3DE render client"
o3de_built=0
o3de_running=0

if [[ -d "$EDITOR_APP" ]] || [[ -x "$EDITOR_BIN" ]]; then
  pass "native Editor build present ($EDITOR_APP)"
  o3de_built=1
elif [[ -d "$O3DE_ROOT/.git" ]]; then
  warn "engine cloned at $O3DE_ROOT but Editor not built yet (see README / setup-o3de-mac.sh)"
else
  warn "no native O3DE at $O3DE_ROOT"
fi

if pgrep -f '/Editor\.app/Contents/MacOS/Editor|O3DE.*Editor' >/dev/null 2>&1; then
  pass "native Editor process running"
  o3de_running=1
elif [[ "$o3de_built" -eq 1 ]]; then
  warn "Editor built but not running (open the app for a live client)"
fi

if command -v docker >/dev/null 2>&1; then
  if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$O3DE_DOCKER_NAME"; then
    pass "Docker O3DE client ($O3DE_DOCKER_NAME) running"
    o3de_built=1
    o3de_running=1
  else
    # only note if profile might be expected
    if docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "$O3DE_DOCKER_NAME"; then
      warn "Docker O3DE client exists but is stopped"
    fi
  fi
fi

if [[ "$o3de_built" -eq 0 ]]; then
  fail "O3DE not ready (build native Editor or start Linux Docker profile o3de-client)"
elif [[ "$STRICT" -eq 1 && "$o3de_running" -eq 0 ]]; then
  fail "O3DE not running (--strict)"
fi


# --- Pixel Streaming ---
# Auto-detect: run the section if the stack is explicitly requested
# (--pixelstreaming / VF_CHECK_PIXELSTREAMING=1) OR if the signalling container
# is already running. When only auto-detected (not explicitly requested), a down
# endpoint is a soft WARN so telemetry-only runs are never broken by it; when
# explicitly requested (or under --strict), it is a hard FAIL.
ps_detected=0
if command -v docker >/dev/null 2>&1 &&
  docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$PS_SIGNALLING_NAME"; then
  ps_detected=1
fi

ps_hard=0
if [[ "$CHECK_PS" -eq 1 ]] || [[ "$STRICT" -eq 1 && "$ps_detected" -eq 1 ]]; then
  ps_hard=1
fi

# Soft/hard problem reporter for this section.
ps_problem() { if [[ "$ps_hard" -eq 1 ]]; then fail "$1"; else warn "$1"; fi; }

if [[ "$CHECK_PS" -eq 1 || "$ps_detected" -eq 1 ]]; then
  echo
  echo "Pixel Streaming (Wilbur signalling $PS_HTTP_URL)"

  ps_signalling_up=0
  if curl -sf --max-time 3 "$PS_HTTP_URL/" >/dev/null; then
    pass "Wilbur player HTTP ($PS_HTTP_URL)"
    ps_signalling_up=1
  else
    ps_problem "Wilbur player HTTP (start: docker compose -f infra/pixelstreaming/docker-compose.yml --profile pixelstreaming up -d)"
  fi

  if port_open 127.0.0.1 "$PS_STREAMER_PORT"; then
    pass "streamer WS port $PS_STREAMER_PORT open"
  else
    ps_problem "streamer WS port $PS_STREAMER_PORT closed (streamers cannot connect)"
  fi

  if port_open 127.0.0.1 "$PS_SFU_PORT"; then
    pass "SFU WS port $PS_SFU_PORT open"
  else
    warn "SFU WS port $PS_SFU_PORT closed (SFU profile is optional; Linux-only)"
  fi

  if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$PS_SIGNALLING_NAME"; then
    pass "container $PS_SIGNALLING_NAME running"
  else
    ps_problem "container $PS_SIGNALLING_NAME not running"
  fi
  if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$PS_STREAMER_NAME"; then
    pass "container $PS_STREAMER_NAME running (reference streamer)"
  else
    warn "reference streamer not running (add --profile streamer to see test output)"
  fi

  # Under --strict, run the harness single-shot as the media-path gate.
  if [[ "$STRICT" -eq 1 ]]; then
    if [[ "$ps_signalling_up" -eq 1 ]] && command -v docker >/dev/null 2>&1; then
      echo "  ..   running reference-streamer harness (drop/reorder/latency gate)"
      # Both profiles are required so the depended-on 'signalling' service is
      # defined; --no-deps reuses the already-running stack instead of starting a
      # duplicate.
      if docker compose -f "$PS_COMPOSE" --profile pixelstreaming --profile streamer \
          run --rm --no-deps \
          -e PS_HARNESS_PLAYER_URL="ws://signalling:80" \
          reference-streamer npm run harness; then
        pass "harness media-path gate (no excess drops/reorder/latency)"
      else
        fail "harness media-path gate (see harness output above)"
      fi
    else
      warn "harness skipped (signalling not up or docker unavailable)"
    fi
  fi
fi

echo
echo "summary: ok=$ok warn=$warn fail=$fail"
if [[ "$fail" -gt 0 ]]; then
  exit 1
fi
exit 0

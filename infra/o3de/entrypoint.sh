#!/usr/bin/env bash
# Stub until the VectorFlow O3DE Gem + engine image exist.
# Domain boundary: this process must never call PromQL / VictoriaMetrics.

set -euo pipefail

echo "vectorflow3d o3de-client stub"
echo "  VF_BRIDGE_VERSION=${VF_BRIDGE_VERSION:-unset}"
echo "  VF_SESSION_MODE=${VF_SESSION_MODE:-unset}"
echo "  VF_SGS_BRIDGE_URL=${VF_SGS_BRIDGE_URL:-unset}"
echo
echo "O3DE does not use PromQL. Telemetry Resolver owns VictoriaMetrics."
echo "Replace this stub with Editor/GameLauncher + VectorFlow Gem (spec Phase 6)."
echo

if [[ "${VF_REQUIRE_GEM:-0}" == "1" ]]; then
  echo "error: VF_REQUIRE_GEM=1 but no Gem binary is installed in this image" >&2
  exit 1
fi

# Keep container healthy for compose scaffolding
while true; do
  sleep 3600
done

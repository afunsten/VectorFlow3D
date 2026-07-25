#!/usr/bin/env bash
# Prepare a native O3DE source tree for VectorFlow3D local client use on macOS.
# Engine lives outside this repo: ~/O3DE/o3de (override with O3DE_ROOT).

set -euo pipefail

O3DE_ROOT="${O3DE_ROOT:-$HOME/O3DE/o3de}"
O3DE_REPO="${O3DE_REPO:-https://github.com/o3de/o3de.git}"
O3DE_BRANCH="${O3DE_BRANCH:-development}"

echo "==> VectorFlow3D — O3DE macOS setup"
echo "    O3DE_ROOT=$O3DE_ROOT"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: missing '$1'. $2" >&2
    exit 1
  fi
}

need git "Install Xcode CLT or Git."
need cmake "brew install cmake (need >= 3.30)."

if ! command -v git-lfs >/dev/null 2>&1; then
  if [[ -x "$HOME/.local/bin/git-lfs" ]]; then
    export PATH="$HOME/.local/bin:$PATH"
  else
    echo "error: git-lfs not found." >&2
    echo "  brew install git-lfs && git lfs install" >&2
    echo "  or install a release binary into ~/.local/bin" >&2
    exit 1
  fi
fi

if ! xcode-select -p >/dev/null 2>&1; then
  echo "error: Xcode developer directory not set. Install Xcode, then: xcode-select -s /Applications/Xcode.app" >&2
  exit 1
fi

git lfs install

mkdir -p "$(dirname "$O3DE_ROOT")"

if [[ ! -d "$O3DE_ROOT/.git" ]]; then
  echo "==> Cloning O3DE ($O3DE_BRANCH) — large download (Git LFS)…"
  git clone --branch "$O3DE_BRANCH" --single-branch "$O3DE_REPO" "$O3DE_ROOT"
else
  echo "==> Existing clone at $O3DE_ROOT"
fi

cd "$O3DE_ROOT"
git lfs pull

if [[ ! -x python/python.sh ]] && [[ -f python/get_python.sh ]]; then
  echo "==> Fetching O3DE Python…"
  ./python/get_python.sh
fi

echo "==> Configuring Xcode project (profile assets = mac)…"
cmake -B build/mac_xcode -S . -G Xcode \
  -DLY_ASSET_DEPLOY_MODE=LOOSE \
  -DLY_ASSET_DEPLOY_ASSET_TYPE=mac

cat <<EOF

==> Configure done.

Build the Editor (long first run):

  cd "$O3DE_ROOT"
  cmake --build build/mac_xcode --target Editor --config profile

Run:

  open "$O3DE_ROOT/build/mac_xcode/bin/profile/Editor.app"

VectorFlow notes:
  - O3DE is a valid local client (vf.bridge.v1 later).
  - Do not point O3DE at VictoriaMetrics / PromQL.
  - Env template: infra/o3de/env.example in the VectorFlow3D repo.

EOF

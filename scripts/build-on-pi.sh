#!/usr/bin/env bash
# Rsync this repo to a 64-bit Raspberry Pi 4 and build natively there.
#
#   ./scripts/build-on-pi.sh user@pi-hostname
#   ./scripts/build-on-pi.sh user@pi-hostname --appimage
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST=""
WANT_APPIMAGE=0

usage() {
  echo "Usage: $0 user@host [--appimage]"
  echo "  Syncs the tree to ~/rustcams on the Pi, installs apt/rust deps,"
  echo "  and runs cargo build --release (not fat LTO)."
}

for arg in "$@"; do
  case "$arg" in
    --appimage) WANT_APPIMAGE=1 ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      if [[ -n "$HOST" ]]; then
        echo "Unknown argument: $arg" >&2
        usage >&2
        exit 1
      fi
      HOST="$arg"
      ;;
  esac
done

if [[ -z "$HOST" ]]; then
  usage >&2
  exit 1
fi

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing command: $1" >&2
    exit 1
  }
}

need_cmd rsync
need_cmd ssh

echo "Syncing to ${HOST}:rustcams/ ..."
rsync -az --delete \
  --exclude target/ \
  --exclude dist/ \
  --exclude .git/ \
  --exclude .deps/ \
  --exclude .cross/ \
  --exclude '*.AppImage' \
  "$ROOT/" "${HOST}:rustcams/"

echo "Building on ${HOST} ..."
# -t so sudo on the Pi can prompt for a password.
ssh -t "$HOST" "WANT_APPIMAGE=${WANT_APPIMAGE} bash -s" <<'REMOTE'
set -euo pipefail
cd "$HOME/rustcams"
chmod +x scripts/setup-pi-deps.sh scripts/package-linux-appimage.sh
./scripts/setup-pi-deps.sh
# shellcheck disable=SC1091
if [[ -f "$HOME/.cargo/env" ]]; then
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

if [[ "$(uname -m)" != "aarch64" ]]; then
  echo "Need 64-bit Pi OS (aarch64), got $(uname -m)." >&2
  exit 1
fi

echo "cargo build --release ..."
cargo build --release

if [[ "${WANT_APPIMAGE}" == "1" ]]; then
  echo "Packaging AppImage (CARGO_PROFILE=release)..."
  CARGO_PROFILE=release ./scripts/package-linux-appimage.sh --skip-build
fi

echo
echo "Binary: $HOME/rustcams/target/release/rustcams"
echo "Run (system GStreamer):"
echo "  cd ~/rustcams && ./target/release/rustcams"
echo "  # or: RUSTCAMS_DECODE=hw ./target/release/rustcams"
echo "Config: cameras.toml in that directory, first argument, or ~/.config/citadel-cctv/"
if [[ "${WANT_APPIMAGE}" == "1" ]]; then
  echo "AppImage: $HOME/rustcams/dist/Citadel_CCTV-linux-aarch64.AppImage"
fi
REMOTE

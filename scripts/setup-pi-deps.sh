#!/usr/bin/env bash
# Install build + runtime packages for Citadel CCTV on 64-bit Raspberry Pi OS
# (Bookworm) or Ubuntu aarch64. Run on the Pi.
#
#   ./scripts/setup-pi-deps.sh
set -euo pipefail

if [[ "$(uname -m)" != "aarch64" ]]; then
  echo "This script targets 64-bit Raspberry Pi OS / Ubuntu (aarch64). Got $(uname -m)." >&2
  echo "32-bit Pi OS is not supported." >&2
  exit 1
fi

if ! command -v apt-get >/dev/null 2>&1; then
  echo "Need apt-get (Raspberry Pi OS, Debian, or Ubuntu)." >&2
  exit 1
fi

export DEBIAN_FRONTEND=noninteractive

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  pkg-config \
  libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad \
  gstreamer1.0-libav \
  gstreamer1.0-tools \
  libegl1 \
  libgl1 \
  libgles2 \
  libgl1-mesa-dri \
  libglib2.0-dev \
  libx11-dev \
  libxcursor-dev \
  libxi-dev \
  libxkbcommon-dev \
  libxrandr-dev \
  libxxf86vm-dev \
  libwayland-dev

if ! command -v cargo >/dev/null 2>&1 && [[ ! -x "${HOME}/.cargo/bin/cargo" ]]; then
  echo "Installing rustup (stable)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi

# shellcheck disable=SC1091
if [[ -f "${HOME}/.cargo/env" ]]; then
  # shellcheck source=/dev/null
  . "${HOME}/.cargo/env"
fi

echo
echo "Deps ready. cargo: $(command -v cargo || echo missing)"
echo "Next: cargo build --release"
echo "For VideoCore decode: RUSTCAMS_DECODE=hw ./target/release/rustcams"

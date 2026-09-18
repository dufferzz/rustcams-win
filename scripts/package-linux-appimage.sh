#!/usr/bin/env bash
# Build a portable Linux AppImage (GStreamer plugins + lib closure).
#
# Prefer Ubuntu 22.04 (Docker / GitHub Actions) so the glibc requirement
# stays old enough for other distros. A Manjaro-built image may fail on Debian.
#
# Usage (from repo root):
#   ./scripts/package-linux-appimage.sh
#   ./scripts/package-linux-appimage.sh --skip-build
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SKIP_BUILD=0
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=1 ;;
    -h|--help)
      echo "Usage: $0 [--skip-build]"
      exit 0
      ;;
    *)
      echo "Unknown argument: $arg" >&2
      exit 1
      ;;
  esac
done

ARCH="$(uname -m)"
if [[ "$ARCH" != "x86_64" ]]; then
  echo "This script currently packages x86_64 only (got $ARCH)." >&2
  exit 1
fi

OUT_DIR="$ROOT/dist"
APPDIR="$OUT_DIR/Citadel_CCTV.AppDir"
TOOLS="$ROOT/.deps"
OUT_APPIMAGE="$OUT_DIR/Citadel_CCTV-linux-x86_64.AppImage"
CARGO_PROFILE="${CARGO_PROFILE:-dist}"
BIN="$ROOT/target/$CARGO_PROFILE/rustcams"

export APPIMAGE_EXTRACT_AND_RUN=1

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing command: $1" >&2
    exit 1
  }
}

need_cmd cargo
need_cmd curl
need_cmd pkg-config
need_cmd sha256sum

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  echo "Building rustcams (--profile $CARGO_PROFILE)..."
  cargo build --profile "$CARGO_PROFILE"
fi

if [[ ! -f "$BIN" ]]; then
  echo "Missing binary: $BIN" >&2
  exit 1
fi

mkdir -p "$TOOLS"
fetch() {
  local url="$1" dest="$2"
  if [[ -f "$dest" ]]; then
    return 0
  fi
  echo "Downloading $(basename "$dest")..."
  curl -L --fail --progress-bar -o "$dest.partial" "$url"
  mv "$dest.partial" "$dest"
  chmod +x "$dest"
}

LINUXDEPLOY="$TOOLS/linuxdeploy-x86_64.AppImage"
PLUGIN_APPIMAGE="$TOOLS/linuxdeploy-plugin-appimage-x86_64.AppImage"
fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage" \
  "$LINUXDEPLOY"
fetch "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-x86_64.AppImage" \
  "$PLUGIN_APPIMAGE"

run_linuxdeploy() {
  # Plugin discovery looks next to the linuxdeploy binary.
  env LINUXDEPLOY="$LINUXDEPLOY" PATH="$TOOLS:$PATH" \
    "$LINUXDEPLOY" --appimage-extract-and-run "$@"
}

find_gst_plugins_dir() {
  local d
  for d in \
    "/usr/lib/${ARCH}-linux-gnu/gstreamer-1.0" \
    /usr/lib/gstreamer-1.0; do
    if [[ -d "$d" ]]; then
      echo "$d"
      return 0
    fi
  done
  echo "Could not find GStreamer plugins (install gst-libav / gstreamer1.0-libav)." >&2
  exit 1
}

find_gst_helpers_dir() {
  local d
  for d in \
    "/usr/lib/${ARCH}-linux-gnu/gstreamer1.0/gstreamer-1.0" \
    /usr/libexec/gstreamer-1.0 \
    /usr/lib/gstreamer-1.0; do
    if [[ -x "$d/gst-plugin-scanner" ]]; then
      echo "$d"
      return 0
    fi
  done
  echo "Could not find gst-plugin-scanner." >&2
  exit 1
}

PLUGINS_SRC="$(find_gst_plugins_dir)"
HELPERS_SRC="$(find_gst_helpers_dir)"

# Mirror scripts/package-windows.ps1 (Linux .so names; no D3D11/MF).
REQUIRED_PLUGINS=(
  libgstcoreelements.so
  libgstapp.so
  libgstrtsp.so
  libgstrtp.so
  libgstrtpmanager.so
  libgstudp.so
  libgsttcp.so
  libgsttypefindfunctions.so
  libgstvideoparsersbad.so
  libgstlibav.so
)
OPTIONAL_PLUGINS=(
  libgstplayback.so
  libgstvideorate.so
  libgstvideoconvertscale.so
  libgstvideoconvert.so
  libgstvideoscale.so
  libgstvideofilter.so
  libgstjpeg.so
)

echo "GStreamer plugins: $PLUGINS_SRC"
echo "GStreamer helpers: $HELPERS_SRC"

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" \
  "$APPDIR/usr/lib/gstreamer-1.0" \
  "$APPDIR/usr/share/citadel-cctv" \
  "$APPDIR/usr/share/licenses/citadel-cctv" \
  "$APPDIR/apprun-hooks"

cp "$BIN" "$APPDIR/usr/bin/rustcams"
chmod +x "$APPDIR/usr/bin/rustcams"
cp "$ROOT/pack/linux/citadel-cctv.desktop" "$APPDIR/citadel-cctv.desktop"
cp "$ROOT/assets/icon.png" "$APPDIR/citadel-cctv.png"
cp "$ROOT/cameras.example.toml" "$APPDIR/usr/share/citadel-cctv/cameras.example.toml"

copy_plugin() {
  local name="$1" required="$2"
  local src="$PLUGINS_SRC/$name"
  if [[ -f "$src" ]]; then
    cp -a "$src" "$APPDIR/usr/lib/gstreamer-1.0/"
    return 0
  fi
  if [[ "$required" -eq 1 ]]; then
    echo "Missing required plugin: $src" >&2
    exit 1
  fi
  echo "Skipping optional plugin: $name"
}

for p in "${REQUIRED_PLUGINS[@]}"; do
  copy_plugin "$p" 1
done
for p in "${OPTIONAL_PLUGINS[@]}"; do
  copy_plugin "$p" 0
done

if [[ ! -f "$APPDIR/usr/lib/gstreamer-1.0/libgstvideoconvertscale.so" \
   && ! -f "$APPDIR/usr/lib/gstreamer-1.0/libgstvideoconvert.so" ]]; then
  echo "Need videoconvert (libgstvideoconvertscale.so or libgstvideoconvert.so)." >&2
  exit 1
fi

cp -a "$HELPERS_SRC/gst-plugin-scanner" "$APPDIR/usr/bin/gst-plugin-scanner"
chmod +x "$APPDIR/usr/bin/gst-plugin-scanner"
if [[ -f "$HELPERS_SRC/gst-ptp-helper" ]]; then
  cp -a "$HELPERS_SRC/gst-ptp-helper" "$APPDIR/usr/bin/gst-ptp-helper" || true
fi

cat > "$APPDIR/apprun-hooks/linuxdeploy-plugin-gstreamer.sh" <<'EOF'
#! /bin/bash
export GST_REGISTRY_REUSE_PLUGIN_SCANNER="no"
export GST_PLUGIN_SYSTEM_PATH_1_0="${APPDIR}/usr/lib/gstreamer-1.0"
export GST_PLUGIN_PATH_1_0="${APPDIR}/usr/lib/gstreamer-1.0"
export GST_PLUGIN_SCANNER_1_0="${APPDIR}/usr/bin/gst-plugin-scanner"
if [ -x "${APPDIR}/usr/bin/gst-ptp-helper" ]; then
  export GST_PTP_HELPER_1_0="${APPDIR}/usr/bin/gst-ptp-helper"
fi
EOF
chmod +x "$APPDIR/apprun-hooks/linuxdeploy-plugin-gstreamer.sh"

# Best-effort license copies (LGPL/GPL when redistributing GStreamer / libav).
shopt -s nullglob
for src in \
  /usr/share/licenses/gstreamer \
  /usr/share/licenses/gst-plugins-base \
  /usr/share/licenses/gst-plugins-good \
  /usr/share/licenses/gst-plugins-bad \
  /usr/share/licenses/gst-libav \
  /usr/share/doc/libgstreamer1.0-0 \
  /usr/share/doc/gstreamer1.0-libav \
  /usr/share/doc/libgstreamer-plugins-base1.0-0; do
  if [[ -e "$src" ]]; then
    cp -a "$src" "$APPDIR/usr/share/licenses/citadel-cctv/" || true
  fi
done
shopt -u nullglob

LD_ARGS=(
  --appdir "$APPDIR"
  --executable "$APPDIR/usr/bin/rustcams"
  --desktop-file "$APPDIR/citadel-cctv.desktop"
  --icon-file "$APPDIR/citadel-cctv.png"
  --exclude-library "libGL.so*"
  --exclude-library "libEGL.so*"
  --exclude-library "libGLdispatch.so*"
  --exclude-library "libGLX.so*"
  --exclude-library "libOpenGL.so*"
  --exclude-library "libvulkan.so*"
  --exclude-library "libdrm.so*"
  --exclude-library "libva.so*"
  --exclude-library "libva-*.so*"
  --exclude-library "libnvidia*"
  --exclude-library "libcuda*"
  --exclude-library "libwayland-*.so*"
)

while IFS= read -r -d '' plug; do
  LD_ARGS+=(--library "$plug")
done < <(find "$APPDIR/usr/lib/gstreamer-1.0" -maxdepth 1 -name 'libgst*.so' -print0)

SCANNER="$APPDIR/usr/bin/gst-plugin-scanner"
if [[ -f "$SCANNER" ]]; then
  LD_ARGS+=(--executable "$SCANNER")
fi
if [[ -f "$APPDIR/usr/bin/gst-ptp-helper" ]]; then
  LD_ARGS+=(--executable "$APPDIR/usr/bin/gst-ptp-helper")
fi

echo "Collecting shared library closure and packing AppImage..."
mkdir -p "$OUT_DIR"
run_linuxdeploy "${LD_ARGS[@]}" --output appimage

shopt -s nullglob
BUILT=( "$OUT_DIR"/Citadel_CCTV*.AppImage "$ROOT"/Citadel_CCTV*.AppImage )
shopt -u nullglob
FOUND=""
for f in "${BUILT[@]:-}"; do
  if [[ -f "$f" && "$f" != "$OUT_APPIMAGE" ]]; then
    FOUND="$f"
    break
  fi
done
if [[ -z "$FOUND" ]]; then
  # Plugin may name it after the desktop file in cwd.
  mapfile -t extra < <(find "$ROOT" "$OUT_DIR" -maxdepth 1 -name '*.AppImage' ! -name 'linuxdeploy*' 2>/dev/null | head -n 20)
  for f in "${extra[@]:-}"; do
    if [[ -f "$f" && "$(basename "$f")" != "linuxdeploy-x86_64.AppImage" ]]; then
      FOUND="$f"
      break
    fi
  done
fi
if [[ -z "$FOUND" ]]; then
  echo "linuxdeploy did not produce an AppImage." >&2
  exit 1
fi

mv -f "$FOUND" "$OUT_APPIMAGE"
chmod +x "$OUT_APPIMAGE"
sha256sum "$OUT_APPIMAGE" | tee "$OUT_APPIMAGE.sha256"

echo
echo "AppImage: $OUT_APPIMAGE"
echo "Place cameras.toml next to the AppImage, or under ~/.config/citadel-cctv/"
echo "GitHub: gh release create vX.Y.Z $OUT_APPIMAGE $OUT_APPIMAGE.sha256"

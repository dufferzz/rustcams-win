#!/usr/bin/env bash
# Cross-build rustcams for Windows (MinGW) from Linux and assemble a portable folder.
#
# Prerequisites:
#   - rustup target x86_64-pc-windows-gnu
#   - mingw-w64-gcc
#   - Unpacked MinGW GStreamer under .cross/gst/mingw_root (see README)
#
# Usage (from repo root):
#   ./scripts/package-windows-cross.sh
#   ./scripts/package-windows-cross.sh --skip-build

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GST_PREFIX="${GST_PREFIX:-$ROOT/.cross/gst/mingw_root/PFiles64/gstreamer/1.0/mingw_x86_64}"
OUT="${OUT:-$ROOT/dist/rustcams}"
TARGET=x86_64-pc-windows-gnu
SKIP_BUILD=0

for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=1 ;;
    *) echo "unknown arg: $arg" >&2; exit 1 ;;
  esac
done

if [[ ! -d "$GST_PREFIX/bin" || ! -d "$GST_PREFIX/lib/pkgconfig" ]]; then
  cat >&2 <<EOF
Missing MinGW GStreamer at:
  $GST_PREFIX

Unpack official MinGW MSIs (runtime + devel) first, e.g.:
  wine msiexec /a gstreamer-1.0-mingw-x86_64-VERSION.msi /qn TARGETDIR=$ROOT/.cross/gst/mingw_root
  wine msiexec /a gstreamer-1.0-devel-mingw-x86_64-VERSION.msi /qn TARGETDIR=$ROOT/.cross/gst/mingw_root
EOF
  exit 1
fi

export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_PATH="$GST_PREFIX/lib/pkgconfig"
export LIBRARY_PATH="$GST_PREFIX/lib${LIBRARY_PATH:+:$LIBRARY_PATH}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export PATH="${HOME}/.cargo/bin:${PATH}"

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  echo "Building --release --target $TARGET ..."
  (cd "$ROOT" && cargo build --release --target "$TARGET")
fi

EXE="$CARGO_TARGET_DIR/$TARGET/release/rustcams.exe"
if [[ ! -f "$EXE" ]]; then
  echo "missing $EXE" >&2
  exit 1
fi

PRESERVE="$(mktemp -d)"
for name in cameras.toml views.toml; do
  if [[ -f "$OUT/$name" ]]; then
    cp -a "$OUT/$name" "$PRESERVE/$name"
  fi
done

rm -rf "$OUT"
mkdir -p "$OUT/gstreamer/bin" "$OUT/gstreamer/lib/gstreamer-1.0" "$OUT/licenses/gstreamer"

echo "Copying rustcams.exe..."
cp -a "$EXE" "$OUT/rustcams.exe"

# Windows loads import DLLs before main(); ship runtime DLLs beside the exe.
echo "Copying GStreamer runtime DLLs next to rustcams.exe..."
cp -a "$GST_PREFIX"/bin/*.dll "$OUT/"

echo "Copying GStreamer bin DLLs/helpers..."
cp -a "$GST_PREFIX"/bin/*.dll "$OUT/gstreamer/bin/"
for helper in gst-plugin-scanner.exe gst-inspect-1.0.exe; do
  if [[ -f "$GST_PREFIX/bin/$helper" ]]; then
    cp -a "$GST_PREFIX/bin/$helper" "$OUT/gstreamer/bin/"
  elif [[ -f "$GST_PREFIX/libexec/gstreamer-1.0/$helper" ]]; then
    cp -a "$GST_PREFIX/libexec/gstreamer-1.0/$helper" "$OUT/gstreamer/bin/"
  fi
done
# Plugin scanner often lives under libexec on this layout
if [[ -f "$GST_PREFIX/libexec/gstreamer-1.0/gst-plugin-scanner.exe" ]]; then
  cp -a "$GST_PREFIX/libexec/gstreamer-1.0/gst-plugin-scanner.exe" "$OUT/gstreamer/bin/"
fi
if [[ -f "$OUT/gstreamer/bin/gst-plugin-scanner.exe" ]]; then
  cp -a "$OUT/gstreamer/bin/gst-plugin-scanner.exe" "$OUT/"
fi

echo "Copying GStreamer plugins..."
cp -a "$GST_PREFIX"/lib/gstreamer-1.0/*.dll "$OUT/gstreamer/lib/gstreamer-1.0/" 2>/dev/null || true

if [[ -f "$ROOT/cameras.example.toml" ]]; then
  cp -a "$ROOT/cameras.example.toml" "$OUT/"
fi

for name in cameras.toml views.toml; do
  if [[ -f "$ROOT/$name" ]]; then
    cp -a "$ROOT/$name" "$OUT/$name"
    echo "  + $name (from repo root)"
  elif [[ -f "$PRESERVE/$name" ]]; then
    cp -a "$PRESERVE/$name" "$OUT/$name"
    echo "  + $name (preserved from previous package)"
  fi
done
rm -rf "$PRESERVE"
if [[ -d "$GST_PREFIX/share/licenses" ]]; then
  cp -a "$GST_PREFIX/share/licenses/." "$OUT/licenses/gstreamer/" 2>/dev/null || true
fi
for f in COPYING LICENSE LICENSE.txt COPYING.LIB; do
  if [[ -f "$GST_PREFIX/$f" ]]; then
    cp -a "$GST_PREFIX/$f" "$OUT/licenses/gstreamer/"
  fi
done

SIZE=$(du -sh "$OUT" | awk '{print $1}')
echo
echo "Portable package ready: $OUT ($SIZE)"
echo "Copy that folder to a Windows PC and run rustcams.exe from inside it."

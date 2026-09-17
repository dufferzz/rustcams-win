# rustcams — common build targets
#
#   make            # release build (Linux)
#   make run        # cargo run --release
#   make windows    # cross-build + portable dist/rustcams/
#   make help

.PHONY: help build release run check clippy clean \
	windows windows-build windows-package windows-gst

CARGO   ?= cargo
TARGET_WIN := x86_64-pc-windows-gnu
GST_VER ?= 1.26.10
GST_DIR := .cross/gst
GST_ROOT := $(GST_DIR)/mingw_root
GST_PREFIX := $(GST_ROOT)/PFiles64/gstreamer/1.0/mingw_x86_64
GST_RUNTIME_MSI := $(GST_DIR)/gstreamer-1.0-mingw-x86_64-$(GST_VER).msi
GST_DEVEL_MSI := $(GST_DIR)/gstreamer-1.0-devel-mingw-x86_64-$(GST_VER).msi
GST_BASE_URL := https://gstreamer.freedesktop.org/data/pkg/windows/$(GST_VER)/mingw

help:
	@echo "Targets:"
	@echo "  build / release   Native release build"
	@echo "  run               Run release build"
	@echo "  check             cargo check"
	@echo "  clippy            cargo clippy -- -D warnings"
	@echo "  clean             cargo clean + remove dist/"
	@echo "  windows           Cross-build + package dist/rustcams/"
	@echo "  windows-build     Cross-compile only (x86_64-pc-windows-gnu)"
	@echo "  windows-package   Package existing Windows exe + GStreamer"
	@echo "  windows-gst       Download + unpack MinGW GStreamer ($(GST_VER))"

build release:
	$(CARGO) build --release

run:
	$(CARGO) run --release

check:
	$(CARGO) check

clippy:
	$(CARGO) clippy -- -D warnings

clean:
	$(CARGO) clean
	rm -rf dist

# --- Windows (MinGW cross from Linux) ---

windows: windows-build windows-package

windows-build: $(GST_PREFIX)/lib/pkgconfig/gstreamer-1.0.pc
	PKG_CONFIG_ALLOW_CROSS=1 \
	PKG_CONFIG_PATH="$(abspath $(GST_PREFIX))/lib/pkgconfig" \
	LIBRARY_PATH="$(abspath $(GST_PREFIX))/lib$${LIBRARY_PATH:+:$$LIBRARY_PATH}" \
	CARGO_TARGET_DIR="$(abspath target)" \
	$(CARGO) build --release --target $(TARGET_WIN)

windows-package: $(GST_PREFIX)/bin
	./scripts/package-windows-cross.sh --skip-build

windows-gst: $(GST_PREFIX)/lib/pkgconfig/gstreamer-1.0.pc
	@echo "GStreamer ready at $(GST_PREFIX)"

$(GST_RUNTIME_MSI) $(GST_DEVEL_MSI): | $(GST_DIR)
	curl -L --fail --progress-bar -o $@ $(GST_BASE_URL)/$(notdir $@)

$(GST_DIR):
	mkdir -p $@

$(GST_PREFIX)/lib/pkgconfig/gstreamer-1.0.pc: $(GST_RUNTIME_MSI) $(GST_DEVEL_MSI)
	@command -v wine >/dev/null || { echo "wine required (pacman -S wine)"; exit 1; }
	mkdir -p $(GST_ROOT)
	WINEDEBUG=-all wine msiexec /a "$(abspath $(GST_RUNTIME_MSI))" /qn \
		TARGETDIR="$(abspath $(GST_ROOT))"
	WINEDEBUG=-all wine msiexec /a "$(abspath $(GST_DEVEL_MSI))" /qn \
		TARGETDIR="$(abspath $(GST_ROOT))"
	@test -f $@

$(GST_PREFIX)/bin: $(GST_PREFIX)/lib/pkgconfig/gstreamer-1.0.pc
	@test -d $@

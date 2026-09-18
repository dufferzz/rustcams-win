# rustcams

Performant multi-camera RTSP CCTV viewer written in Rust.

- Custom **views** with layout + camera slots (saved in `views.toml`)
- Left **camera list** — drag onto grid cells
- Drag cells to **reorder / swap** streams
- Right-click a cell to clear it
- Fit modes: Contain / Cover / Fill
- Hover for camera name; status bar (CPU, RAM, network) while the perf overlay is open
- Background pause when unfocused
- Optional **Hikvision NVR** discovery: list cameras via ISAPI, stream through the NVR

Streaming pipeline (RTSP → decode → egui): [docs/streaming.md](docs/streaming.md).

## Requirements

### Linux (Arch/Manjaro)

```bash
sudo pacman -S gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav
# Optional NVIDIA HW decode:
sudo pacman -S gst-plugin-nvcodec
```

Portable **AppImage** (for GitHub releases; build on Ubuntu 22.04 when possible):

```bash
./scripts/package-linux-appimage.sh
# → dist/Citadel_CCTV-linux-x86_64.AppImage
```

`chmod +x` the file and run it. Put `cameras.toml` next to the AppImage, pass the path as the first argument, or use `~/.config/citadel-cctv/cameras.toml`. This image is built on the local distro (glibc must be ≥ the builder’s); it is not a GitHub Actions artifact.

Publish a GitHub release from this machine (tag must already exist):

```bash
gh release create v0.1.0 \
  dist/Citadel_CCTV-linux-x86_64.AppImage \
  dist/Citadel_CCTV-linux-x86_64.AppImage.sha256
```

### Windows

#### Develop / build on Windows (MSVC)

Primary path on Windows: MSVC Rust + MSVC GStreamer, then:

```powershell
.\scripts\build-release.ps1 -Quick
```

Day-to-day: incremental **release** build (no LTO), then refresh only `dist\rustcams\rustcams.exe`
(keeps already-bundled GStreamer DLLs; skips the ZIP). First time, or after changing
the plugin allowlist / GStreamer install, or when shipping a ZIP:

```powershell
.\scripts\build-release.ps1
```

That full script loads the Visual Studio / GStreamer environment, builds the **dist**
profile (fat LTO), creates `dist\rustcams\`, writes `dist\rustcams-windows-x64.zip`,
and prints its SHA-256. Pass `-Clean` to force a clean Cargo rebuild, or `-SkipZip`
to skip compressing the archive. For package-only, use `.\scripts\package-windows.ps1`
(`-SkipBuild`, optional `-Quick` / `-CargoProfile`).

Do **not** mix MinGW GStreamer with `x86_64-pc-windows-msvc` (ABI mismatch).

1. Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) (C++ workload) and a recent `x86_64-pc-windows-msvc` Rust toolchain.
2. Install both [GStreamer MSVC 64-bit](https://gstreamer.freedesktop.org/download/) packages (same version; **Complete** recommended while developing):
   - Runtime
   - Development
3. Optional manual env (usually unnecessary when using `build-release.ps1`):

```bat
set PATH=C:\gstreamer\1.0\msvc_x86_64\bin;%PATH%
set PKG_CONFIG_PATH=C:\gstreamer\1.0\msvc_x86_64\lib\pkgconfig
set LIB=C:\gstreamer\1.0\msvc_x86_64\lib;%LIB%
```

(Adjust the prefix if your installer used a different path; the Runtime/Devel installers also set `GSTREAMER_1_0_ROOT_MSVC_X86_64`.)

```bat
cargo build --release
```

#### Cross-build from Linux (Arch/Manjaro)

Produces a MinGW `rustcams.exe` plus bundled GStreamer under `dist/rustcams/`.

1. Install toolchain:
   ```bash
   sudo pacman -S mingw-w64-gcc wine
   rustup target add x86_64-pc-windows-gnu
   ```
2. Download matching **MinGW** runtime + devel MSIs from
   [gstreamer.freedesktop.org](https://gstreamer.freedesktop.org/download/) into `.cross/gst/`.
3. Unpack both into the same tree (one-time):
   ```bash
   mkdir -p .cross/gst/mingw_root
   wine msiexec /a .cross/gst/gstreamer-1.0-mingw-x86_64-VERSION.msi /qn \
     TARGETDIR="$(pwd)/.cross/gst/mingw_root"
   wine msiexec /a .cross/gst/gstreamer-1.0-devel-mingw-x86_64-VERSION.msi /qn \
     TARGETDIR="$(pwd)/.cross/gst/mingw_root"
   ```
4. Build + package:
   ```bash
   make windows
   # or: ./scripts/package-windows-cross.sh
   ```
   First time only, fetch/unpack MinGW GStreamer: `make windows-gst` (or let `make windows` do it).

Copy `dist/rustcams/` to a Windows PC and run `rustcams.exe` from that folder (no system GStreamer install required). Use **MinGW** GStreamer with the `x86_64-pc-windows-gnu` Rust target only — not MSVC packages.

#### Portable layout

Both packaging scripts produce `dist/rustcams/` with:

```text
rustcams.exe
*.dll                          # runtime closure only (beside exe — required on Windows)
gst-plugin-scanner.exe
cameras.example.toml
gstreamer\bin\                 # helpers (scanner / gst-inspect)
gstreamer\lib\gstreamer-1.0\   # allowlisted plugins (RTSP/decode/D3D11/…)
licenses\gstreamer\
```

`package-windows.ps1` copies **only** the plugins rustcams needs (RTSP, RTP depay, H.264/H.265 parse, libav + D3D11/Media Foundation decoders, convert/scale, JPEG) and the runtime DLL dependency closure of the exe + those plugins — not the full GStreamer install. On Windows, runtime `*.dll` must sit next to `rustcams.exe` because the loader resolves imports before `main` can adjust `PATH`. At startup, rustcams sets `GST_PLUGIN_PATH` to `gstreamer\lib\gstreamer-1.0`. MSVC builds may still need the [Visual C++ Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist) on target PCs.

GStreamer / codecs have LGPL/GPL obligations when redistributing — keep the copied license files with the package.

**Decode (default = software):** rustcams uses an **explicit** pipeline (`rtph264depay` → `h264parse` → decoder), not `decodebin`. By default it selects **libav** (`avdec_h264` / `avdec_h265`), which measured smoother than DXVA on multi-cam grids (lower emit gaps around keyframes). Set `RUSTCAMS_DECODE=hw` to prefer Direct3D11/DXVA (`d3d11h264dec` / `d3d11h265dec`) with GPU scale/download when `gstd3d11.dll` is present. In **Debug** (`D` or `RUSTCAMS_DEBUG=1`), per-stream `dec=` shows the factory in use; `stutter-stats.log` next to the exe records `gap_ms` every ~2 s.

## Config

```bash
cp cameras.example.toml cameras.toml
# edit — cameras.toml is gitignored
```

Launch without a file: rustcams starts with a warning; add cameras by editing `cameras.toml`.

Views are auto-saved next to the config as `views.toml` whenever you change
layout, drag cameras, or click a library camera into a selected cell. The last
selected view is remembered in `ui.toml` and restored on launch. Portable
Windows builds load `cameras.toml` / `views.toml` next to `rustcams.exe`.
AppImages use `cameras.toml` next to the `.AppImage` file if present, otherwise
`~/.config/citadel-cctv/` (so views can be saved; the image itself is read-only).

### Shape

```toml
[nvr]
host = "198.51.100.20"
http_port = 49000
rtsp_port = 49002
username = "admin"
password = "..."
stream = "sub"       # main | sub | third
protocols = "udp"    # udp | tcp | udp+tcp

[[cameras]]
id = "front_ptz"
name = "Front PTZ"
url = "rtsp://admin:pass@192.0.2.10:554/Streaming/Channels/102"

[[cameras]]
id = "barracks"
name = "Front"
url = "rtsp://admin:pass@192.0.2.10:554/Streaming/Channels/201"

[viewer]
default_layout = "3x3"
default_fit = "fill"   # contain | cover | fill
pause_when_unfocused = false
```

Each `[[cameras]]` entry needs `id` and a camera-direct RTSP `url` (credentials
embedded). There is no shared `[ptz]` section. Set `ptz = true` to enable PTZ
when the id/name does not contain `ptz` (`ptz = false` turns it off).

### Direct cameras

Omit `[nvr]`. Grid and fullscreen both use each camera’s `url`.

### Hikvision NVR mode

Add an `[nvr]` block. On startup rustcams calls
`GET /ISAPI/ContentMgmt/InputProxy/channels` (HTTP digest) and builds grid RTSP
URLs through the NVR:

```text
rtsp://user:pass@NVR:{rtsp_port}/Streaming/Channels/{channelNo * 100 + streamType}
```

`streamType`: `1` main, `2` sub, `3` third (config `stream = "main"|"sub"|"third"`).

Listed cameras keep stable ids (for `views.toml`) by matching the host and lens
in `url` (`/Streaming/Channels/101` → lens 1, `…/201` → lens 2), or an explicit
`channel = N`. Unmatched discovered channels get slug ids from the NVR name.
Fullscreen / PTZ use the camera `url` (rewritten to main stream).

## Run

```bash
cargo run --release
```

### Perf / debug

| Knob | Effect |
|------|--------|
| Toolbar **Debug** or `D` | On-screen per-stream fps, size, decoder, stale frames, RGBA copy cost, emit `gap_ms`; status-bar UI/tex rates |
| `RUSTCAMS_DEBUG=1` | Starts with Debug on; logs `perf summary` / `perf stream` / `perf ui` every 5s |
| `RUSTCAMS_DECODE=hw` | Force hardware decode (D3D11/MF/NV); default is software `avdec_*` |
| `RUSTCAMS_DECODE=sw` | Explicit software decode (same as default) |
| `stutter-stats.log` | Always written beside the exe (~2 s); use `gap_ms` / `dec=` to compare hitch |
| `RUST_LOG=rustcams=debug` | Verbose module logs (links, stops, …) |
| `GST_DEBUG=2` or `GST_DEBUG=rtsp*:3,videodecoder:3` | GStreamer-side RTSP/decode traces |

```bash
RUSTCAMS_DEBUG=1 RUST_LOG=rustcams=info cargo run --release
```

Deep dive: [docs/streaming.md](docs/streaming.md).
## Controls

| Action | How |
|--------|-----|
| Switch / create views | Toolbar dropdown, ➕, ✎ Rename, 🗑 Delete |
| Layout | Toolbar `1` / `2` / `2×2` / `3×3`… (`2` = stacked dual / vertical split; `2×2` is toolbar-only) |
| Fit mode | Settings — Contain / Cover / Fill (`F` still cycles) |
| HD | Toolbar toggle — main `…01` / `101` vs sub `…02` / `102` (1 / 2 / 2×2 only; denser grids force sub) |
| Assign camera | Drag from left list onto a cell, or click a list camera to replace the selected cell |
| Select camera | Click a cell (or the library) — accent border; arms PTZ when the camera has it |
| Reorder | Drag a cell onto another cell (swap) |
| Clear cell | Right-click |
| Fullscreen | Double-click / `Esc` — switches that cam to **direct main** RTSP; keeps PTZ on that cam |
| Full screen | ⛶ toolbar — true OS/monitor fullscreen; `Esc` exits (after camera FS) |
| Settings | ⚙ toolbar — fit, accent, outline width, perf overlay, log console |
| Debug | Settings or `D` — perf overlay + stream decode metrics; status bar while open |
| Log | Settings or `L` — in-app log console (Windows release builds hide the OS console) |
| PTZ pan / tilt | Sidebar pad (including diagonals) or arrow keys (**selected camera**) |
| PTZ zoom | Sidebar `−` / `+`, or `=` / `+` / PageUp in; `-` / PageDown out |
| PTZ home | Sidebar `⌂` or `H` |
| PTZ park action | Sidebar **Park On/Off** — idle return to preset/patrol; click to toggle |
| PTZ tracking | Sidebar **Tracking On/Off** — intrusion detection on the camera; click to toggle |
| PTZ (DualShock 4) | Left stick pan/tilt; L2/R2 or right-stick Y zoom; L1/R1 focus; Cross = preset 1; Triangle = patrol 1 |

PTZ talks **directly to each camera** (`http://{camera-host}:80/ISAPI/PTZCtrl/…`),
not through the NVR. Digest auth is warmed when you **select** a PTZ camera.
Credentials and host come from that camera’s RTSP `url`. Speeds default to move
30 / zoom 25. Cameras whose id/name contain `ptz`, or that set `ptz = true`,
get a PTZ target from `url`.

**Fullscreen** switches that camera to its **direct main-stream** URL (same `url`
rewritten to `…01`) at higher decode width (1280). Pipelines always use
`appsink sync=false` with a ~450–500 ms RTSP jitterbuffer (no `videorate`);
grid panes cap size/fps by layout. Exit fullscreen returns to the NVR substream when `[nvr]` is set.

## Notes

- Only cameras in the active view are decoded. With **D3D11 plugins present**, 1×1 and camera-fullscreen stop off-screen streams to save CPU/GPU; otherwise fullscreen keeps the rest running so exit is instant.
- Prefer substreams (`stream = "sub"`, `/…02`) for grid viewing.
- Dense grids (3×3+) always use substreams and a moderate decode width (e.g. 5×5 → 352 px) so OSD timestamps stay readable; **HD** (main stream) is only available on 1×1 / 2×2 and fullscreen.
- Default RTSP transport is **UDP** (LAN-friendly) unless you set `protocols` in config. On failure, reconnects alternate to **TCP** when protocols are not pinned. Some Hikvision cams return SETUP **500** with TCP interleaved — pin `protocols = "udp"` in `cameras.toml` if needed.
- Default video decode is **software** (`avdec_*`). Use `RUSTCAMS_DECODE=hw` only if you want DXVA and accept possible GOP hitch; compare with `stutter-stats.log` (`gap_ms`).
- Opening many streams to one camera IP can hit its concurrent-session limit; use fewer slots or substreams only.
- In NVR mode, concurrent viewers hit **one** NVR. Prefer substreams and fewer slots; the NVR’s own session limits still apply.

Pipeline details: [docs/streaming.md](docs/streaming.md).

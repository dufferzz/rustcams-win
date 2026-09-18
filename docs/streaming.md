# Streaming: connection to display

How Citadel CCTV (rustcams) takes an RTSP camera from config to pixels on screen.

Video is **RTSP only**. HTTP is used for Hikvision NVR discovery (ISAPI) and PTZ, not for media. The UI is **eframe/egui + glow (OpenGL)** — frames are CPU RGBA uploaded as egui textures, not shared GPU surfaces with the decoder.

---

## Big picture

```
cameras.toml  →  resolve URLs
      ↓
ViewerApp::desired_streams()   (layout / HD / fullscreen tiers)
      ↓
StreamManager::sync_active()   (start / stop / reconnect pipelines)
      ↓
Per camera GStreamer pipeline:
  rtspsrc → rtph26xdepay → h26xparse → decoder
    → [optional D3D11 postproc when HW decode]
    → queue → videoconvert → videoscale → capsfilter(RGBA) → appsink
      ↓
appsink callback: keyframe gate → copy RGBA → latest-frame ring
      ↓
UI thread (~16 ms): frame_if_newer → ColorImage → TextureHandle
      ↓
paint grid / fullscreen cell → egui painter.image()
```

| Stage | Owner | Main files |
|-------|--------|------------|
| Config / URLs | `ViewerApp` + config resolve | `config.rs`, `nvr.rs` |
| Which streams to run | UI each frame | `app/mod.rs` |
| Pipeline lifecycle | `StreamManager` | `stream.rs` |
| Explicit decode linking | GStreamer pad-added | `gst_link.rs`, `gst_env.rs` |
| Texture upload + paint | UI thread | `app/mod.rs`, `app/ui.rs` |

There is **no Tokio/async runtime** for video. Sync Rust on the UI thread drives sync/upload; GStreamer uses its own streaming/decode threads for the pipeline and appsink callback.

---

## 1. Bootstrap

On startup (`main.rs` → `ViewerApp::new` → `StreamManager::new`):

1. `configure_bundled_gstreamer()` points at the exe-adjacent `gstreamer/` tree (packaged builds).
2. `gst::init()`.
3. Hardware decoder ranks are adjusted (`prefer_hardware_decoders`) and D3D11 postproc availability is probed (`d3d11_postproc_available`).

See `gst_env.rs`.

**Default decode path is software (`avdec_h264` / `avdec_h265`).** DXVA/`d3d11h264dec` caused ~300–900 ms emit gaps around GOP boundaries on this workload; libav is smoother. Set `RUSTCAMS_DECODE=hw` to force hardware again.

---

## 2. Establishing the RTSP URL

Media never opens a custom TCP stack for video. The app resolves an RTSP URL, then hands it to GStreamer’s `rtspsrc`.

### Direct mode (`[nvr]` absent)

Each `[[cameras]]` entry supplies `url = "rtsp://..."`. Resolve produces a `CameraConfig` with:

- `url` — used for the grid
- `direct_url` — often a rewrite toward the camera’s main stream (used in fullscreen when set)
- optional `protocols` (`udp` / `tcp` / `udp+tcp`)

### Hikvision NVR mode (`[nvr]` present)

1. HTTP digest `GET` to  
   `http://{host}:{http_port}/ISAPI/ContentMgmt/InputProxy/channels`
2. Parse XML channels; match `[[cameras]]` overrides by channel, host+lens, or name.
3. Build the grid RTSP URL:

   ```
   rtsp://user:pass@NVR:{rtsp_port}/Streaming/Channels/{channelId*100 + streamDigit}
   ```

   Stream digits: `1` = main, `2` = sub, `3` = third (`nvr::build_rtsp_url`).

4. Fullscreen may switch to `direct_url` (camera LAN main stream) when available.
5. PTZ prefers camera `http://{cam}:80/ISAPI/PTZCtrl/…`, then NVR `PTZCtrlProxy` / `PTZCtrl` with the InputProxy channel id.

### Stream digit rewriting

`nvr::rewrite_stream_digit` rewrites the last digit of `/Streaming/Channels/N` to the tier’s `StreamType` (main/sub/third). That is how layout/HD changes quality without rebuilding the whole camera list.

---

## 3. Deciding what to decode

Every UI frame (`eframe::App::update`):

1. Focus / minimize + `pause_when_unfocused` → whether streams are **active**.
2. `sync_streams(active)` → `streams.sync_active(&desired)`.
3. `update_textures(ctx)`.
4. Paint grid or fullscreen.

### `StreamRequest`

Built in `ViewerApp::desired_streams()` (`app/mod.rs`):

| Field | Meaning |
|-------|---------|
| `id` | Camera id |
| `url` | Final RTSP URL (after digit rewrite) |
| `protocols` | Optional transport override |
| `max_width` | Cap after scale (height capped to same value) |
| `max_fps` | Tier hint (substream fps); not a `videorate` element |

### Which cameras get pipelines

- Normally: every slotted camera in the **active view**.
- **Solo decode** (D3D11 available + 1×1 layout or camera fullscreen): only visible camera(s), so off-screen streams stop and free GPU/CPU.
- Without solo mode, the whole view keeps decoding so leaving fullscreen is instant.

### Quality tiers (`stream_tier`)

| Context | max_width | max_fps | Stream |
|---------|-----------|---------|--------|
| Fullscreen HD | 1280 | 30 | Main |
| Fullscreen no HD | 640 | 15 | Sub |
| 1×1 HD / Sub | 1280@30 / 640@15 | Main / Sub |
| Two HD / Sub | 960@20 / 640@15 | Main / Sub |
| 2×2 HD / Sub | 640@15 | Main / Sub |
| 3×3 | 480 @ 15 | **Sub only** |
| 4×4 | 400 @ 15 | **Sub only** |
| 5×5 | 352 @ 15 | **Sub only** |
| 6×6 | 288 @ 15 | **Sub only** |

Dense-grid widths are sized so on-screen OSD (date/time) stays readable while keeping substreams.

Fullscreen prefers `direct_url` with protocols cleared (UDP default unless `protocols` is set). Grid uses the NVR/grid URL and configured protocols.

HD is only offered for layouts One / Two / Grid2; denser grids force substream + the pixel budgets above.

---

## 4. Pipeline graph

Built in `start_pipeline` (`stream.rs`). One pipeline per active camera.

**No `decodebin` and no `videorate`.** Linking is explicit on `rtspsrc` pad-added (`gst_link::link_explicit_video`).

```
[Network RTSP]
      │
   rtspsrc
      │ pad-added (H264 / H265 RTP)
   rtph264depay / rtph265depay
      → h264parse / h265parse
      → decoder
           default: avdec_h264 / avdec_h265
           RUSTCAMS_DECODE=hw: d3d11h264dec / … (MF / NV fallbacks)
      │
      ├── HW + D3D11 plugins: d3d11convert → d3d11scale → caps(RGBA) → d3d11download
      └── else: direct link from decoder
      ▼
 queue (leaky downstream, max 4 buffers)
      → videoconvert → videoscale → capsfilter (RGBA, ≤ max w/h)
      → appsink
```

### `rtspsrc` knobs (hardcoded)

| Property | Value |
|----------|--------|
| `latency` | 450 ms (≤640 px) / 500 ms (wider) — smooths GOP/jitter |
| `protocols` | udp or tcp (or multi-protocol override from config) |
| `drop-on-latency` | **false** (dropping on latency clipped around IDRs → hitch) |
| `do-retransmission` | false |
| `do-rtsp-keep-alive` | true |
| `timeout` / `tcp-timeout` | 5 s |

### Decode / display knobs

- **Scale method:** `nearest-neighbour` if `max_width ≤ 400`, else `bilinear`.
- **Clock sync on appsink:** always `sync=false`. Clock sync held frames then dropped late ones in bursts (~GOP hitch).
- **appsink:** `max-buffers=1`, `drop=true`, `max-lateness=-1`, `qos=false`.
- **Keyframe gate:** after connect/reconnect, delta/corrupt buffers are dropped until the first keyframe (queue probe + appsink). Upstream `ForceKeyUnit` is sent to request an IDR ASAP.
- **Audio:** non-video RTP pads are ignored (`gst_link::is_audio_caps`).

### Decoder selection (`gst_link.rs` / env)

| `RUSTCAMS_DECODE` | Behavior |
|-------------------|----------|
| unset / `sw` / `software` / `avdec` | Software (`avdec_*`) — **default** |
| `hw` / `hardware` / `d3d11` | Prefer `d3d11h264dec` / `d3d11h265dec`, then MF/NV, else SW |

When HW decode is used and `d3d11convert` / `d3d11scale` / `d3d11download` exist, frames go through GPU convert/scale to RGBA in `D3D11Memory`, then download into the leaky queue. On link failure, decoder links straight to the queue (CPU convert/scale).

`prefer_hardware_decoders` still ranks HW factories above SW for any path that uses ranks; the explicit graph ignores rank when `RUSTCAMS_DECODE` forces SW.

The selected decoder name is stored on the slot and shown in the debug overlay / stutter log (`dec=`).

---

## 5. Frame handoff (appsink → ring)

On GStreamer’s streaming thread, the appsink `new_sample` callback:

1. Pulls the sample; applies keyframe / corrupt filters.
2. Maps plane 0 as readable RGBA.
3. Copies into a contiguous `Vec<u8>` (`copy_rgba_plane`, respects stride).
4. Allocates a global monotonic `seq` and wraps `VideoFrame { width, height, rgba, seq }` in an `Arc`.
5. Stores it in `slot.frame: Arc<Mutex<Option<Arc<VideoFrame>>>>`, replacing any unread frame (**stale**).
6. Only then stores `latest_seq` with `Release` ordering — so the UI never sees a new sequence number paired with old pixels.
7. Updates emit-gap counters (`emit_gap_max_ms`) for stutter diagnosis.

```rust
// Conceptual shape
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,  // opaque A=255
    pub seq: u64,
}
```

There is **no shared GPU texture** between decoder and UI. The UI always uploads CPU RGBA via egui/glow.

---

## 6. Threading model

| Actor | Thread | Responsibility |
|-------|--------|----------------|
| `ViewerApp` | UI / egui (~60 Hz) | Config, views, textures; calls `sync_active` / `frame_if_newer` |
| `StreamManager` | API on UI thread; pipelines live under it | `HashMap` of `SlotState` |
| GStreamer | Internal streaming / decode threads | Elements + appsink callback |
| Bus poll | UI thread inside `sync_active` | Non-blocking `bus.pop()` for Error / Eos |
| `PtzWorker` | Dedicated `ptz-worker` thread | ISAPI HTTP for PTZ (not on the video path) |

Handoff is lock + atomics only: appsink writes the latest frame; UI clones the `Arc` (or `try_unwrap`s it to avoid a pixel copy when unique).

---

## 7. Display (egui)

`update_textures` (`app/mod.rs`):

1. For each **displayed** camera, call `frame_if_newer(id, tex.seq)`.
2. Prefer `Arc::try_unwrap` so RGBA ownership moves into the `ColorImage` without cloning; otherwise clone pixels.
3. Transmute RGBA → `Color32` (opaque) → `ColorImage`.
4. `TextureHandle::set` or `ctx.load_texture`.
5. Dense grids (3×3–6×6) use `TextureOptions::NEAREST`; others use `LINEAR`.
6. Upload every tile that has a newer frame (uploads are cheap relative to decode).
7. `paint_cell_contents` / `draw_grid` draw with `painter.image()` and fit mode Contain / Cover / Fill.

Repaint cadence: ~16 ms when active, ~250 ms when paused.

Cell placeholders: Connecting… / Reconnecting… / Error / Offline / Paused (`app/ui.rs`).

### Stutter / perf logging

While Debug is on (toolbar / `D` / `RUSTCAMS_DEBUG=1`), the overlay shows per-stream rates. Independently, the app appends a snapshot every ~2 s to **`stutter-stats.log`** next to the executable (e.g. `dist/rustcams/stutter-stats.log`):

- UI / texture FPS and upload cost
- Per cam: `out` / `in` fps, stale, drops, **`gap_ms`** (worst emit spacing), `copy_us`, `dec=`

`gap_ms` is the main hitch metric: with SW decode, steady panes are typically ~100–180 ms at 15 fps; HW decode often showed 300–900 ms around keyframes.

---

## 8. Reconnect and errors

Not a formal state machine — slot flags and timers:

```
Running (pipeline Some)
    → bus Error / Eos
    → stop pipeline
    → failures++
    → maybe flip UDP ↔ TCP
    → schedule_reconnect
No pipeline + reconnect_at ≤ now
    → start_pipeline again
start_pipeline fails
    → schedule_reconnect
```

**Backoff** (`schedule_reconnect`): failures 1 → 2 s, 2 → 5 s, 3–5 → 10 s, else 20 s.

**Transport flip:** only if protocols are **not** pinned to a single mode **and** `failures >= 2`. Multi-protocol config like `udp+tcp` is set directly on `rtspsrc`; reconnect still starts on UDP then can flip.

**Config change** (url / protocols / max_width / max_fps): stop, reset failures, restart immediately.

**Pause:** optional `stop_all` + clear textures when unfocused/minimized; restart on focus.

Errors are shortened for the UI (`short_error` maps common RTSP/auth failures). Credentials are stripped from logs via `redact_url`.

---

## 9. Multi-camera / views

| Piece | Role |
|-------|------|
| `Layout` | 1, 2 (stacked), 2×2 … 6×6 (`layout.rs`) |
| `View.slots` | Which camera id sits in which cell (`views.toml`) |
| Active view only | Only that view’s slotted cameras are requested |
| Fullscreen | Double-click / Cross(X) → higher tier (+ direct URL when available); Esc / right-click / Triangle exits |
| Select / PTZ | Click a cell (or library) → amber border; PTZ pad/keys target that camera |
| DnD | Library → cell, cell ↔ cell swap (`app/ui.rs`) |

---

## Ownership summary

```
ViewerApp (UI thread)
  ├── cameras: Vec<CameraConfig>      // resolved URLs / PTZ metadata
  ├── views: ViewStore                // which cam in which cell
  ├── streams: StreamManager          // GStreamer + frame rings
  │     └── slots[id]: SlotState
  │           pipeline, frame ring, latest_seq, error,
  │           transport, reconnect_at, failures, counters
  ├── textures: HashMap<id, TexCache> // egui GPU textures + last seq
  └── ptz: PtzWorker                  // separate thread; selected-camera control
```

- Connection lifecycle / reconnect → `StreamManager`
- Which URLs and tiers → `ViewerApp`
- Pixel buffer until upload → `Arc<VideoFrame>` in the slot
- GPU texture → egui `TexCache` on the UI thread

---

## Config that affects streaming

### `[nvr]`

`host`, `http_port`, `rtsp_port`, `username`, `password`, `stream` (main/sub/third), `protocols`

### `[[cameras]]`

`id`, `name`, `url`, `channel`, `stream`, `protocols`

### `[viewer]`

`app_name`, `default_layout`, `default_fit`, **`pause_when_unfocused`**

### Runtime UI (not TOML)

- HD toolbar toggle
- Layout buttons
- Camera fullscreen
- Fit: contain / cover / fill

### Environment

| Variable | Effect |
|----------|--------|
| `RUSTCAMS_DECODE` | `hw` = force D3D11/MF/NV; default / `sw` = libav |
| `RUSTCAMS_DEBUG` | Perf overlay on at start; periodic `perf *` logs |
| `RUST_LOG`, `GST_DEBUG` | Module / GStreamer traces |
| Bundled GStreamer | Exe-relative tree on Windows packages |

---

## Edge cases

| Topic | Behavior |
|-------|----------|
| Reconnect | Backoff + UDP↔TCP after repeated failures unless protocols pinned |
| RTSP 500 on some Hikvision cams | Often TCP interleaved issues — pin `protocols = "udp"` |
| Pause when unfocused | Stops all pipelines and clears textures when enabled |
| Audio | Explicitly ignored |
| HTTP video | Not supported |
| Stale frames | Ring overwritten before UI read — counted in debug / stutter log |
| Many streams to one NVR/IP | Session limits can fail; keep grids modest or use substreams |
| Fullscreen vs grid URL | Fullscreen may jump to direct main; exit restores NVR/grid URL |
| Dense grids | Forced substream + max_width above; HD disabled |
| D3D11 solo decode | Stops off-screen pipelines in 1×1 / fullscreen when D3D11 postproc exists |
| Green flash on connect | Keyframe gate + ForceKeyUnit |
| Clock sync | Always off on appsink (avoids GOP hitch) |
| HW vs SW stutter | Prefer SW default; use `gap_ms` in `stutter-stats.log` to compare |

---

## Key source index

| Path | Role |
|------|------|
| `src/stream.rs` | `StreamManager`, `SlotState`, `VideoFrame`, pipeline, appsink, reconnect |
| `src/gst_link.rs` | Explicit depay/parse/decode, D3D11 postproc, audio skip |
| `src/gst_env.rs` | Bundled plugins, HW decoder ranks |
| `src/app/mod.rs` | Sync, tiers, texture upload, update loop, stutter log path |
| `src/app/ui.rs` | Grid paint, placeholders, debug overlay, stutter-stats writer |
| `src/config.rs` | TOML → `CameraConfig` |
| `src/nvr.rs` | ISAPI discovery, RTSP URL build/rewrite |
| `src/views.rs`, `src/layout.rs` | Multi-view grid model |
| `src/ptz.rs` | Parallel HTTP control (not decode) |
| `src/main.rs` | eframe entry, config load |

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
  rtspsrc → decodebin → [optional D3D11 postproc] → queue → videorate
  → videoconvert → videoscale → capsfilter(RGBA) → appsink
      ↓
appsink callback: copy RGBA → latest-frame ring (Arc<VideoFrame>)
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
| Decode pad linking | GStreamer callbacks | `gst_link.rs`, `gst_env.rs` |
| Texture upload + paint | UI thread | `app/mod.rs`, `app/ui.rs` |

There is **no Tokio/async runtime** for video. Sync Rust on the UI thread drives sync/upload; GStreamer uses its own streaming/decode threads for the pipeline and appsink callback.

---

## 1. Bootstrap

On startup (`main.rs` → `ViewerApp::new` → `StreamManager::new`):

1. `configure_bundled_gstreamer()` points at the exe-adjacent `gstreamer/` tree (packaged builds).
2. `gst::init()`.
3. Hardware decoder ranks are adjusted (`prefer_hardware_decoders`) and D3D11 postproc availability is probed (`d3d11_postproc_available`).

See `gst_env.rs`.

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
| `max_fps` | `videorate` max-rate |

### Which cameras get pipelines

- Normally: every slotted camera in the **active view**.
- **Solo decode** (D3D11 + 1×1 layout or camera fullscreen): only visible camera(s), so off-screen streams stop and free GPU.
- Without solo mode, the whole view keeps decoding so leaving fullscreen is instant.

### Quality tiers (`stream_tier`)

| Context | max_width | max_fps | Stream |
|---------|-----------|---------|--------|
| Fullscreen HD | 1280 | 30 | Main |
| Fullscreen no HD | 640 | 15 | Sub |
| 1×1 HD / Sub | 1280@30 / 640@15 | Main / Sub |
| Two HD / Sub | 960@20 / 640@15 | Main / Sub |
| 2×2 HD / Sub | 640@15 | Main / Sub |
| 3×3 … 6×6 | 288…192 @ 15 | **Sub only** |

Fullscreen prefers `direct_url` with protocols cleared (UDP default). Grid uses the NVR/grid URL and configured protocols.

HD is only offered for layouts One / Two / Grid2; denser grids force substream + small pixel budgets.

---

## 4. Pipeline graph

Built in `start_pipeline` (`stream.rs`). One pipeline per active camera.

```
[Network RTSP]
      │
   rtspsrc
      │ pad-added
   decodebin  ──► HW decoder (d3d11h264dec / …) or SW
      │
      ├── D3D11 path (when plugins exist):
      │     d3d11convert → d3d11scale → caps(RGBA) → d3d11download
      │
      └── else: direct link
      ▼
 queue (leaky, max 2 buffers)
      → videorate (drop-only, max-rate)
      → videoconvert → videoscale → capsfilter (RGBA, ≤ max w/h)
      → appsink
```

### `rtspsrc` knobs (hardcoded)

| Property | Value |
|----------|--------|
| `latency` | 200 ms |
| `protocols` | udp or tcp (or multi-protocol override) |
| `drop-on-latency` | true |
| `do-retransmission` | false |
| `do-rtsp-keep-alive` | true |
| `timeout` / `tcp-timeout` | 5 s |

### Decode / display knobs

- **Scale method:** `nearest-neighbour` if `max_width ≤ 400`, else `bilinear`.
- **Clock sync on appsink:** only when `max_width > 640` (HD-ish). Smaller panes set `sync=false` to prefer the latest frame over clock alignment.
- **appsink:** `max-buffers=1`, `drop=true`, `max-lateness=100ms`, `qos=false`.
- **videorate:** must be `drop-only` + `max-rate`. Classic rate + framerate caps asserts on live RTSP buffers that lack duration.
- **Audio:** audio pads from `decodebin` are ignored (`gst_link::is_audio_caps`).

### D3D11 post-process (`gst_link.rs`)

When `d3d11convert`, `d3d11scale`, and `d3d11download` exist, decodebin video is linked through GPU convert/scale to RGBA in `D3D11Memory`, then downloaded as a small CPU frame into the leaky queue. Downstream CPU convert/scale become cheap passthroughs. On link failure, the code falls back to `decodebin → queue` (full CPU path).

Hardware decoder factories are ranked up (`d3d11h*`, `mfh*`, `nvh*`, `vah*`); `avdec_h264/265` are demoted. The selected decoder name is logged and exposed in the debug overlay.

---

## 5. Frame handoff (appsink → ring)

On GStreamer’s streaming thread, the appsink `new_sample` callback:

1. Pulls the sample and maps plane 0 as readable RGBA.
2. Copies into a contiguous `Vec<u8>` (`copy_rgba_plane`, respects stride).
3. Allocates a global monotonic `seq` and wraps `VideoFrame { width, height, rgba, seq }` in an `Arc`.
4. Stores it in `slot.frame: Arc<Mutex<Option<Arc<VideoFrame>>>>`, replacing any unread frame (**stale**).
5. Only then stores `latest_seq` with `Release` ordering — so the UI never sees a new sequence number paired with old pixels.

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
6. `paint_cell_contents` / `draw_grid` draw with `painter.image()` and fit mode Contain / Cover / Fill.

Repaint cadence: ~16 ms when active, ~250 ms when paused.

Cell placeholders: Connecting… / Error / Offline / Paused (`app/ui.rs`).

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

**Backoff** (`schedule_reconnect`): failures 1 → 2 s, 2 → 5 s, 3–5 → 10 s, else 20 s.

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
| Fullscreen | Double-click cell → higher tier (+ direct URL when available); Esc exits |
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
  └── ptz: PtzWorker                  // separate thread; fullscreen control
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

- `RUSTCAMS_DEBUG` — perf overlay + periodic logs
- `RUST_LOG`, `GST_DEBUG`
- Bundled GStreamer via exe-relative tree on Windows packages

---

## Edge cases

| Topic | Behavior |
|-------|----------|
| Reconnect | Backoff + UDP↔TCP after repeated failures unless protocols pinned |
| RTSP 500 on some Hikvision cams | Often TCP interleaved issues — pin `protocols = "udp"` |
| Pause when unfocused | Stops all pipelines and clears textures when enabled |
| Audio | Explicitly ignored |
| HTTP video | Not supported |
| Stale frames | Ring overwritten before UI read — counted in debug overlay |
| Many streams to one NVR/IP | Session limits can fail; keep grids modest or use substreams |
| Fullscreen vs grid URL | Fullscreen may jump to direct main; exit restores NVR/grid URL |
| Dense grids | Forced substream + tiny max_width; HD disabled |
| D3D11 solo decode | Stops off-screen pipelines in 1×1 / fullscreen |
| Clock sync | Only for wider (HD-ish) panes |

---

## Key source index

| Path | Role |
|------|------|
| `src/stream.rs` | `StreamManager`, `SlotState`, `VideoFrame`, pipeline, appsink, reconnect |
| `src/gst_link.rs` | Decodebin linking, D3D11 postproc, audio skip |
| `src/gst_env.rs` | Bundled plugins, HW decoder ranks |
| `src/app/mod.rs` | Sync, tiers, texture upload, update loop |
| `src/app/ui.rs` | Grid paint, placeholders, debug overlay |
| `src/config.rs` | TOML → `CameraConfig` |
| `src/nvr.rs` | ISAPI discovery, RTSP URL build/rewrite |
| `src/views.rs`, `src/layout.rs` | Multi-view grid model |
| `src/ptz.rs` | Parallel HTTP control (not decode) |
| `src/main.rs` | eframe entry, config load |

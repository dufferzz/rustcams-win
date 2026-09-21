# Agent notes

## Windows build (default to Quick)

When the user asks to **build** / **rebuild** on Windows, prefer:

```powershell
.\scripts\build-release.ps1 -Quick
```

Do **not** hand-roll `cargo` + GStreamer DLL copy steps. Do **not** use `-Clean` or a full (non-Quick) build unless the user asks for a clean/shipping/ZIP package, or DLLs/plugins are missing/wrong.

| Command | When |
|---------|------|
| `.\scripts\build-release.ps1 -Quick` | Day-to-day (default) |
| `.\scripts\build-release.ps1` | Shipping: fat LTO + full DLL package + ZIP |
| `.\scripts\build-release.ps1 -Clean` | Only if user wants a clean rebuild |
| `.\scripts\build-release.ps1 -SkipZip` | Full package without recreating the ZIP |

**Run from:** `dist\rustcams\rustcams.exe` (bundled DLLs). Not bare `target\release\rustcams.exe` / `target\dist\rustcams.exe` unless GStreamer is already on `PATH`.

### Why builds used to be slow

1. **Cargo:** `[profile.release]` previously had fat `lto = true` + `codegen-units = 1`, which forced every crate through linker-plugin LTO (~5 min). That is **no longer** the default release profile — do not put fat LTO back on `release`.
2. **Packaging:** Full package wipes `dist\` and re-copies all GStreamer plugins/DLLs, then zips. `-Quick` only refreshes `rustcams.exe` and skips the ZIP.

### Cargo profiles (`Cargo.toml`)

| Profile | Flags | Used by |
|---------|--------|---------|
| `release` | `opt-level = 3`, **no LTO**, strip | `-Quick` → `target\release\rustcams.exe` |
| `dist` | inherits release + **fat LTO** + `codegen-units = 1` | full `build-release.ps1` → `target\dist\rustcams.exe` |

### Package script

`scripts/package-windows.ps1`:

- `-SkipBuild` — exe already built
- `-Quick` — overwrite exe only; keep existing GStreamer tree under `dist\rustcams\`
- `-CargoProfile release|dist` — which `target\<profile>\rustcams.exe` to copy

Details: [`.cursor/rules/windows-build.mdc`](.cursor/rules/windows-build.mdc), Windows section of [`README.md`](README.md).

## App display name

User-facing name defaults to **Citadel CCTV**, editable via TOML:

```toml
[viewer]
app_name = "Citadel CCTV"
```

Accent color (selection outline) is set in **Settings** and saved in `ui.toml` next to `cameras.toml`. The last selected view is stored there too and restored on launch.

Crate/binary/dist folder names stay `rustcams` (packaging identity, not branding).

## Streaming / decode (keep docs in sync)

Default decode is **software** (`avdec_*`) via an **explicit** depay/parse chain — not `decodebin`, and not `videorate`. Settings → Decoder can select **NVDEC** or **Auto hardware**; `RUSTCAMS_DECODE=nvdec` / `hw` / `sw` overrides that. Dense-grid tiers are sized for readable OSD (e.g. 5×5 → 352 px).

When changing the pipeline, update:

- [`docs/streaming.md`](docs/streaming.md) — source of truth for RTSP → pixels
- [`README.md`](README.md) — user-facing decode / debug / Notes
- `scripts/package-windows.ps1` plugin allowlist comment if elements change
- `scripts/package-linux-appimage.sh` plugin allowlist if elements change

## Linux AppImage (GitHub release)

```bash
./scripts/package-linux-appimage.sh
# → dist/Citadel_CCTV-linux-x86_64.AppImage   (on x86_64; dist profile)
# → dist/Citadel_CCTV-linux-aarch64.AppImage  (on Pi; release profile)
```

x86_64 shipping uses the **dist** profile; package on Ubuntu 22.04 when possible, then `gh release create` with the files under `dist/`. There is no GitHub Actions release job. Config: `cameras.toml` beside the AppImage or `~/.config/citadel-cctv/`.

## Raspberry Pi 4 (64-bit, SSH)

From the PC:

```bash
./scripts/build-on-pi.sh user@pi
# make pi HOST=user@pi
# ./scripts/build-on-pi.sh user@pi --appimage
```

On the Pi: `./scripts/setup-pi-deps.sh` then `cargo build --release`. Run `target/release/rustcams` with system GStreamer. Use `RUSTCAMS_DECODE=hw` for V4L2. Optional AppImage on-device with `CARGO_PROFILE=release`.

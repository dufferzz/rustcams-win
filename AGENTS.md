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

User-facing name defaults to **Citadel CCTV**, editable via Settings or TOML:

```toml
[viewer]
app_name = "Citadel CCTV"
```

Crate/binary/dist folder names stay `rustcams` (packaging identity, not branding).

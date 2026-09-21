# Build a portable rustcams folder with a minimal bundled MSVC GStreamer runtime.
#
# Prerequisites (Windows):
#   - Rust MSVC toolchain (x86_64-pc-windows-msvc)
#   - GStreamer MSVC 64-bit Runtime + Development (same version)
#   - pkg-config / pkgconf on PATH; PKG_CONFIG_PATH set to GStreamer lib\pkgconfig
#
# Usage (from repo root; prefer .\scripts\build-release.ps1 which loads MSVC env):
#   .\scripts\package-windows.ps1
#   .\scripts\package-windows.ps1 -SkipBuild
#   .\scripts\package-windows.ps1 -SkipBuild -Quick   # refresh exe only (keep bundled DLLs)
#
# Output: dist\rustcams\  (zip-friendly; run rustcams.exe from that folder)
#
# Only RTSP / H.264 / H.265 / D3D11 (plus small fallbacks) plugins are copied.
# Runtime DLLs beside the exe are the closure of rustcams.exe + those plugins
# (Windows loads import DLLs before main() can fix PATH).
#
# -Quick skips wiping dist and re-copying GStreamer plugins/DLLs when a prior
# package already exists -- use after normal code changes. Full package when the
# allowlist, GStreamer install, or DLL set may have changed.

[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$Quick,
    [string]$CargoProfile = "release",
    [string]$OutDir = "dist\rustcams"
)

$ErrorActionPreference = "Stop"

function Resolve-GStreamerRoot {
    if ($env:GSTREAMER_1_0_ROOT_MSVC_X86_64) {
        return $env:GSTREAMER_1_0_ROOT_MSVC_X86_64.TrimEnd('\')
    }
    $candidates = @(
        "C:\gstreamer\1.0\msvc_x86_64\1.0\msvc_x86_64",
        "C:\gstreamer\1.0\msvc_x86_64",
        "C:\Program Files\gstreamer\1.0\msvc_x86_64"
    )
    foreach ($c in $candidates) {
        if (Test-Path (Join-Path $c "bin")) {
            return $c
        }
    }
    throw @"
GStreamer MSVC root not found.
Install the official MSVC 64-bit Runtime (and Development for building),
or set GSTREAMER_1_0_ROOT_MSVC_X86_64 to the install prefix.
Do not use the MinGW GStreamer packages with an MSVC Rust toolchain.
"@
}

function Find-Dumpbin {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $install = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
        if ($install) {
            $cand = Get-ChildItem (Join-Path $install "VC\Tools\MSVC") -Recurse -Filter "dumpbin.exe" -ErrorAction SilentlyContinue |
                Where-Object { $_.FullName -match '\\Hostx64\\x64\\dumpbin\.exe$' } |
                Select-Object -First 1
            if ($cand) { return $cand.FullName }
        }
    }
    $fallback = Get-ChildItem "C:\Program Files*\Microsoft Visual Studio" -Recurse -Filter "dumpbin.exe" -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\Hostx64\\x64\\dumpbin\.exe$' } |
        Select-Object -First 1
    if ($fallback) { return $fallback.FullName }
    return $null
}

function Get-PeDependentDllNames([string]$Dumpbin, [string]$PePath) {
    $names = New-Object System.Collections.Generic.List[string]
    $out = & $Dumpbin /DEPENDENTS $PePath 2>$null | Out-String
    foreach ($line in ($out -split "`r?`n")) {
        $t = $line.Trim()
        if ($t -match '^[A-Za-z0-9_\-\.]+\.dll$') {
            $names.Add($t.ToLowerInvariant())
        }
    }
    return $names
}

function Collect-RuntimeDllClosure {
    param(
        [string]$Dumpbin,
        [string]$BinDir,
        [string[]]$RootPePaths
    )
    $sysSkip = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($s in @(
            'kernel32.dll', 'user32.dll', 'gdi32.dll', 'shell32.dll', 'shlwapi.dll', 'ole32.dll', 'oleaut32.dll',
            'advapi32.dll', 'ntdll.dll', 'ws2_32.dll', 'iphlpapi.dll', 'pdh.dll', 'psapi.dll', 'powrprof.dll',
            'bcrypt.dll', 'bcryptprimitives.dll', 'crypt32.dll', 'secur32.dll', 'winmm.dll', 'imm32.dll',
            'dwmapi.dll', 'uxtheme.dll', 'opengl32.dll', 'dxgi.dll', 'd3d11.dll', 'd3d12.dll', 'd3d9.dll',
            'dcomp.dll', 'dxva2.dll', 'mfplat.dll', 'mf.dll', 'mfreadwrite.dll', 'mfuuid.dll', 'propsys.dll',
            'uiautomationcore.dll', 'cfgmgr32.dll', 'setupapi.dll', 'version.dll', 'wintrust.dll',
            'vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll', 'concrt140.dll', 'ucrtbase.dll',
            'msvcrt.dll', 'combase.dll', 'rpcrt4.dll', 'sechost.dll', 'clbcatq.dll'
        )) { [void]$sysSkip.Add($s) }

    $needed = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $queue = New-Object System.Collections.Generic.Queue[string]
    foreach ($p in $RootPePaths) {
        if (Test-Path $p) { $queue.Enqueue((Resolve-Path $p).Path) }
    }

    while ($queue.Count -gt 0) {
        $pe = $queue.Dequeue()
        foreach ($dep in (Get-PeDependentDllNames $Dumpbin $pe)) {
            if ($sysSkip.Contains($dep)) { continue }
            if ($dep.StartsWith('api-ms-win-', [StringComparison]::OrdinalIgnoreCase)) { continue }
            if ($dep.StartsWith('ext-ms-', [StringComparison]::OrdinalIgnoreCase)) { continue }
            $src = Join-Path $BinDir $dep
            if (-not (Test-Path $src)) { continue }
            if ($needed.Add($dep)) {
                $queue.Enqueue($src)
            }
        }
    }
    return @($needed)
}

# Plugins required for rustcams RTSP → depay/parse/decode → RGBA appsink path.
# Include nvcodec when present so Settings → NVDEC works on NVIDIA GPUs.
$PluginAllowList = @(
    'gstcoreelements.dll',      # queue, capsfilter, ...
    'gstapp.dll',               # appsink
    'gstplayback.dll',          # decodebin
    'gstrtsp.dll',              # rtspsrc
    'gstrtp.dll',               # rtph264/h265 depay
    'gstrtpmanager.dll',
    'gstudp.dll',
    'gsttcp.dll',
    'gsttypefindfunctions.dll',
    'gstvideoparsersbad.dll',   # h264parse / h265parse
    'gstvideorate.dll',
    'gstvideoconvertscale.dll', # videoconvert + videoscale
    'gstvideofilter.dll',
    'gstd3d11.dll',             # DXVA decode + convert/scale/download
    'gstlibav.dll',             # avdec_* software fallback
    'gstmediafoundation.dll',   # mfh264/h265 fallback
    'gstnvcodec.dll',           # NVIDIA NVDEC (optional; skipped if missing)
    'gstjpeg.dll'
)

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $repoRoot

$gstRoot = Resolve-GStreamerRoot
Write-Host "GStreamer root: $gstRoot"

$binDir = Join-Path $gstRoot "bin"
$pluginDir = Join-Path $gstRoot "lib\gstreamer-1.0"
if (-not (Test-Path $pluginDir)) {
    throw "Missing plugin directory: $pluginDir (install the Complete MSVC runtime)"
}

# Ensure the linker and pkg-config can see GStreamer during cargo build.
$env:PATH = "$binDir;$env:PATH"
if (-not $env:PKG_CONFIG_PATH) {
    $env:PKG_CONFIG_PATH = Join-Path $gstRoot "lib\pkgconfig"
}
if ($env:LIB) {
    $env:LIB = "$(Join-Path $gstRoot 'lib');$env:LIB"
} else {
    $env:LIB = Join-Path $gstRoot "lib"
}

if (-not $SkipBuild) {
    Write-Host "Building profile '$CargoProfile'..."
    cargo build --profile $CargoProfile
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --profile $CargoProfile failed"
    }
}

function Resolve-RustcamsExe([string]$Profile) {
    $candidates = @()
    if ($env:CARGO_TARGET_DIR) {
        $candidates += (Join-Path $env:CARGO_TARGET_DIR "$Profile\rustcams.exe")
    }
    $candidates += (Join-Path $repoRoot "target\$Profile\rustcams.exe")
    # Older default if profile was left as release.
    if ($Profile -ne "release") {
        if ($env:CARGO_TARGET_DIR) {
            $candidates += (Join-Path $env:CARGO_TARGET_DIR "release\rustcams.exe")
        }
        $candidates += (Join-Path $repoRoot "target\release\rustcams.exe")
    }
    foreach ($c in $candidates) {
        if (Test-Path $c) { return (Resolve-Path $c).Path }
    }
    return $candidates[0]
}

$exe = Resolve-RustcamsExe $CargoProfile
if (-not (Test-Path $exe)) {
    throw "Missing $exe - build first or omit -SkipBuild"
}
Write-Host "Using exe: $exe"

$out = Join-Path $repoRoot $OutDir

function Test-ExistingPackage([string]$Dir) {
    $plugins = Join-Path $Dir "gstreamer\lib\gstreamer-1.0"
    if (-not (Test-Path $plugins)) { return $false }
    $pluginCount = @(Get-ChildItem $plugins -Filter "gst*.dll" -File -ErrorAction SilentlyContinue).Count
    $dllCount = @(Get-ChildItem $Dir -Filter "*.dll" -File -ErrorAction SilentlyContinue).Count
    return ($pluginCount -ge 5) -and ($dllCount -ge 10)
}

function Sync-PackageConfigs([string]$PackageDir) {
    $example = Join-Path $repoRoot "cameras.example.toml"
    if (Test-Path $example) {
        Copy-Item $example (Join-Path $PackageDir "cameras.example.toml") -Force
    }
    foreach ($name in @("cameras.toml", "views.toml")) {
        $dest = Join-Path $PackageDir $name
        $fromRoot = Join-Path $repoRoot $name
        # Keep the packaged view layout (camera slots) — do not clobber with repo views.toml.
        if ($name -eq "views.toml" -and (Test-Path $dest)) {
            Write-Host "  = $name (keeping packaged layout)"
            continue
        }
        if (Test-Path $fromRoot) {
            Copy-Item $fromRoot $dest -Force
            Write-Host "  + $name (from repo root)"
        }
    }
}

$doQuick = [bool]$Quick
if ($doQuick -and -not (Test-ExistingPackage $out)) {
    Write-Host "Quick package requested, but $OutDir is missing or incomplete -- doing a full package."
    $doQuick = $false
}

if ($doQuick) {
    Write-Host "Quick package: refreshing rustcams.exe (keeping existing GStreamer DLLs)..."
    Copy-Item $exe (Join-Path $out "rustcams.exe") -Force
    Sync-PackageConfigs $out
    $sizeMb = [math]::Round(((Get-ChildItem -Recurse $out | Measure-Object -Property Length -Sum).Sum / 1MB), 1)
    Write-Host ""
    Write-Host "Portable package updated: $out - $sizeMb MB (quick)"
    Write-Host "Run rustcams.exe from that folder."
    return
}

$dumpbin = Find-Dumpbin
if (-not $dumpbin) {
    throw "dumpbin.exe not found (need VS C++ tools to compute the runtime DLL closure)"
}
Write-Host "dumpbin: $dumpbin"

# Preserve user config across wipe (cameras.toml / views.toml).
$preserveDir = Join-Path $env:TEMP ("rustcams-preserve-" + [guid]::NewGuid().ToString("n"))
New-Item -ItemType Directory -Path $preserveDir | Out-Null
foreach ($name in @("cameras.toml", "views.toml")) {
    $existing = Join-Path $out $name
    if (Test-Path $existing) {
        Copy-Item $existing (Join-Path $preserveDir $name) -Force
    }
}

if (Test-Path $out) {
    Remove-Item -Recurse -Force $out
}
New-Item -ItemType Directory -Path $out | Out-Null
$gstOut = Join-Path $out "gstreamer"
$gstPluginOut = Join-Path $gstOut "lib\gstreamer-1.0"
$gstBinOut = Join-Path $gstOut "bin"
New-Item -ItemType Directory -Path $gstPluginOut | Out-Null
New-Item -ItemType Directory -Path $gstBinOut | Out-Null

Write-Host "Copying rustcams.exe..."
Copy-Item $exe (Join-Path $out "rustcams.exe")

Write-Host "Copying allowlisted GStreamer plugins ($($PluginAllowList.Count))..."
$copiedPlugins = @()
foreach ($plug in $PluginAllowList) {
    $src = Join-Path $pluginDir $plug
    if (Test-Path $src) {
        Copy-Item $src $gstPluginOut
        $copiedPlugins += (Join-Path $gstPluginOut $plug)
        Write-Host "  + $plug"
    } else {
        Write-Host "  - missing (skipped): $plug"
    }
}

Write-Host "Computing runtime DLL closure..."
$roots = @((Join-Path $out "rustcams.exe")) + $copiedPlugins
$runtimeNames = Collect-RuntimeDllClosure -Dumpbin $dumpbin -BinDir $binDir -RootPePaths $roots
Write-Host "Copying $($runtimeNames.Count) runtime DLLs next to rustcams.exe..."
foreach ($name in ($runtimeNames | Sort-Object)) {
    Copy-Item (Join-Path $binDir $name) (Join-Path $out $name)
}

# Scanner lives under gstreamer\bin (and beside exe) for GST_PLUGIN_SCANNER discovery.
foreach ($h in @("gst-plugin-scanner.exe", "gst-inspect-1.0.exe")) {
    $src = Join-Path $binDir $h
    if (-not (Test-Path $src)) {
        $alt = Join-Path $gstRoot "libexec\gstreamer-1.0\$h"
        if (Test-Path $alt) { $src = $alt }
    }
    if (Test-Path $src) {
        Copy-Item $src $gstBinOut
        if ($h -eq "gst-plugin-scanner.exe") {
            Copy-Item $src (Join-Path $out $h)
        }
        Write-Host "  + helper $h"
    }
}

Sync-PackageConfigs $out

# Restore cameras.toml from prior dist when repo root has none.
# Always restore views.toml from the previous package so camera layouts survive a wipe.
foreach ($name in @("cameras.toml", "views.toml")) {
    $dest = Join-Path $out $name
    $fromPrev = Join-Path $preserveDir $name
    if (-not (Test-Path $fromPrev)) { continue }
    $preferPrev = ($name -eq "views.toml") -or (-not (Test-Path $dest))
    if ($preferPrev) {
        Copy-Item $fromPrev $dest -Force
        Write-Host "  + $name (preserved from previous package)"
    }
}
Remove-Item -Recurse -Force $preserveDir -ErrorAction SilentlyContinue

# License texts from the GStreamer install (LGPL/GPL redistrib obligations).
$licenseOut = Join-Path $out "licenses\gstreamer"
New-Item -ItemType Directory -Path $licenseOut -Force | Out-Null
$licenseHints = @(
    (Join-Path $gstRoot "share\licenses"),
    (Join-Path $gstRoot "share\doc"),
    (Join-Path $gstRoot "share\gst-plugins-base"),
    (Join-Path $gstRoot "COPYING"),
    (Join-Path $gstRoot "LICENSE"),
    (Join-Path $gstRoot "LICENSE.txt")
)
foreach ($hint in $licenseHints) {
    if (Test-Path $hint -PathType Container) {
        Copy-Item $hint (Join-Path $licenseOut (Split-Path $hint -Leaf)) -Recurse -Force -ErrorAction SilentlyContinue
    } elseif (Test-Path $hint -PathType Leaf) {
        Copy-Item $hint $licenseOut -Force -ErrorAction SilentlyContinue
    }
}

$sizeMb = [math]::Round(((Get-ChildItem -Recurse $out | Measure-Object -Property Length -Sum).Sum / 1MB), 1)
$plugMb = [math]::Round(((Get-ChildItem $gstPluginOut -File | Measure-Object Length -Sum).Sum / 1MB), 1)
$dllMb = [math]::Round(((Get-ChildItem $out -Filter "*.dll" -File | Measure-Object Length -Sum).Sum / 1MB), 1)
Write-Host ""
Write-Host "Portable package ready: $out - $sizeMb MB"
Write-Host "  runtime DLLs beside exe: $($runtimeNames.Count) - $dllMb MB"
Write-Host "  plugins: $($copiedPlugins.Count) - $plugMb MB"
Write-Host "Run rustcams.exe from that folder. Copy cameras.example.toml to cameras.toml and edit."
Write-Host "Target PCs may also need the Visual C++ Redistributable if MSVC runtime DLLs are missing."

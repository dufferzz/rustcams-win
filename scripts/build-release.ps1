<#
.SYNOPSIS
Build and package a portable Windows release.

.DESCRIPTION
Sets up the MSVC and GStreamer environments, builds rustcams in release mode,
creates the portable dist\rustcams folder, and recreates the release ZIP.
This script works from a normal PowerShell window; Developer PowerShell is not
required.

.EXAMPLE
.\scripts\build-release.ps1

.EXAMPLE
.\scripts\build-release.ps1 -Clean

.EXAMPLE
.\scripts\build-release.ps1 -Quick
# Incremental Cargo build + refresh dist\rustcams\rustcams.exe only (no DLL recopy / ZIP).
#>

[CmdletBinding()]
param(
    [switch]$Clean,
    [switch]$Quick,
    [switch]$SkipZip,
    [string]$OutDir = "dist\rustcams",
    [string]$ZipPath = "dist\rustcams-windows-x64.zip"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $repoRoot

function Resolve-GStreamerRoot {
    $candidates = @()
    if ($env:GSTREAMER_1_0_ROOT_MSVC_X86_64) {
        $candidates += $env:GSTREAMER_1_0_ROOT_MSVC_X86_64.TrimEnd('\')
    }
    $candidates += @(
        "C:\gstreamer\1.0\msvc_x86_64\1.0\msvc_x86_64",
        "C:\gstreamer\1.0\msvc_x86_64",
        "C:\Program Files\gstreamer\1.0\msvc_x86_64"
    )

    foreach ($candidate in $candidates) {
        if ((Test-Path (Join-Path $candidate "bin")) -and
            (Test-Path (Join-Path $candidate "lib\pkgconfig"))) {
            return (Resolve-Path $candidate).Path
        }
    }

    throw @"
GStreamer MSVC development files were not found.
Install the matching 64-bit MSVC Runtime and Development packages, or set
GSTREAMER_1_0_ROOT_MSVC_X86_64 to their installation prefix.
"@
}

function Import-MsvcEnvironment {
    if ((Get-Command cl.exe -ErrorAction SilentlyContinue) -and
        (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        return
    }

    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) {
        throw "vswhere.exe was not found. Install Visual Studio Build Tools with the C++ workload."
    }

    $install = (& $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath 2>$null | Select-Object -First 1)
    if (-not $install) {
        throw "Visual Studio C++ x64 build tools were not found."
    }

    $vcvars = Join-Path $install "VC\Auxiliary\Build\vcvars64.bat"
    if (-not (Test-Path $vcvars)) {
        throw "Missing MSVC environment script: $vcvars"
    }

    Write-Host "Loading MSVC x64 environment..."
    # Use call so vcvars can chain; keep the whole command in one string so
    # paths with spaces (Program Files (x86)) parse correctly.
    $environment = & cmd.exe /c "call `"$vcvars`" >nul && set"
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to load the MSVC x64 environment."
    }

    foreach ($line in $environment) {
        $separator = $line.IndexOf('=')
        if ($separator -le 0) {
            continue
        }
        $name = $line.Substring(0, $separator)
        $value = $line.Substring($separator + 1)
        [Environment]::SetEnvironmentVariable($name, $value, "Process")
    }
}

function Resolve-RepoPath([string]$Path) {
    if ([IO.Path]::IsPathRooted($Path)) {
        return [IO.Path]::GetFullPath($Path)
    }
    return [IO.Path]::GetFullPath((Join-Path $repoRoot $Path))
}

Import-MsvcEnvironment

$gstRoot = Resolve-GStreamerRoot
$gstBin = Join-Path $gstRoot "bin"
$gstLib = Join-Path $gstRoot "lib"
$gstInclude = Join-Path $gstRoot "include"
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = $gstRoot
$env:PATH = "$gstBin;$env:USERPROFILE\.cargo\bin;$env:PATH"
$env:PKG_CONFIG_PATH = Join-Path $gstLib "pkgconfig"
$env:LIB = if ($env:LIB) { "$gstLib;$env:LIB" } else { $gstLib }
$env:INCLUDE = if ($env:INCLUDE) { "$gstInclude;$env:INCLUDE" } else { $gstInclude }

Write-Host "Repository: $repoRoot"
Write-Host "GStreamer: $gstRoot"

if ($Clean -and $Quick) {
    throw "Use either -Clean (full rebuild + full package) or -Quick (incremental), not both."
}

if ($Clean) {
    Write-Host "Cleaning Cargo output..."
    & cargo clean
    if ($LASTEXITCODE -ne 0) {
        throw "cargo clean failed with exit code $LASTEXITCODE"
    }
}

if ($Quick) {
    $cargoProfile = "release"
    Write-Host "Building release profile (fast; no LTO)..."
} else {
    $cargoProfile = "dist"
    Write-Host "Building dist profile (LTO shipping build)..."
}
& cargo build --profile $cargoProfile
if ($LASTEXITCODE -ne 0) {
    throw "cargo build --profile $cargoProfile failed with exit code $LASTEXITCODE"
}

$resolvedOutDir = Resolve-RepoPath $OutDir
$running = Get-Process rustcams -ErrorAction SilentlyContinue | Where-Object {
    try {
        $_.Path -and $_.Path.StartsWith($resolvedOutDir, [StringComparison]::OrdinalIgnoreCase)
    } catch {
        $false
    }
}
if ($running) {
    Write-Host "Stopping packaged rustcams process so its files can be replaced..."
    $running | Stop-Process -Force
    $running | Wait-Process -ErrorAction SilentlyContinue
}

$packageArgs = @{
    SkipBuild    = $true
    OutDir       = $OutDir
    CargoProfile = $cargoProfile
}
if ($Quick) {
    $packageArgs.Quick = $true
    Write-Host "Creating portable package (quick: exe only)..."
} else {
    Write-Host "Creating portable package..."
}
& (Join-Path $PSScriptRoot "package-windows.ps1") @packageArgs
if ($LASTEXITCODE -ne 0) {
    throw "package-windows.ps1 failed with exit code $LASTEXITCODE"
}

$wantZip = -not $SkipZip -and -not $Quick
if (-not $wantZip) {
    if ($Quick) {
        Write-Host "Skipping ZIP (quick builds omit it; pass a full build without -Quick to recreate the archive)."
    } elseif ($SkipZip) {
        Write-Host "Skipping ZIP (-SkipZip)."
    }
    Write-Host ""
    Write-Host "Release ready"
    Write-Host "  Folder: $resolvedOutDir"
    exit 0
}

$resolvedZipPath = Resolve-RepoPath $ZipPath
$zipParent = Split-Path $resolvedZipPath -Parent
New-Item -ItemType Directory -Path $zipParent -Force | Out-Null
if (Test-Path $resolvedZipPath) {
    Remove-Item $resolvedZipPath -Force
}

Write-Host "Compressing release..."
Compress-Archive -Path $resolvedOutDir -DestinationPath $resolvedZipPath -CompressionLevel Optimal

$zip = Get-Item $resolvedZipPath
if ($zip.Length -eq 0) {
    throw "Release ZIP is empty: $resolvedZipPath"
}
$hash = (Get-FileHash $resolvedZipPath -Algorithm SHA256).Hash
$sizeMb = [math]::Round($zip.Length / 1MB, 1)

Write-Host ""
Write-Host "Release ready"
Write-Host "  Folder: $resolvedOutDir"
Write-Host "  ZIP:    $resolvedZipPath ($sizeMb MB)"
Write-Host "  SHA256: $hash"

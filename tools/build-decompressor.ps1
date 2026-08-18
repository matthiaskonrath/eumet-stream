# Build EUMETSAT's xRITDecompress from source.
#
# SEVIRI HRIT pixel data is wavelet-compressed, and the only implementation is
# EUMETSAT's own (Apache 2.0). It is C++, so it is built once and invoked as a
# helper. Everything else in this project is Rust.
#
#   powershell -File tools\build-decompressor.ps1
#
# The result is dropped in tools\xRITDecompress.exe, where the server finds it
# automatically. Set XRIT_DECOMPRESS to override the location.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$vendor = Join-Path $root "vendor\PublicDecompWT"

if (-not (Test-Path $vendor)) {
    Write-Host "Cloning PublicDecompWT..."
    New-Item -ItemType Directory -Force (Join-Path $root "vendor") | Out-Null
    git clone --depth 1 https://gitlab.eumetsat.int/open-source/PublicDecompWT.git $vendor
}

# Locate the MSVC environment. Visual Studio does not put cl.exe on PATH.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsPath = $null
if (Test-Path $vswhere) {
    $vsPath = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
}
if (-not $vsPath) {
    $vsPath = Get-ChildItem "$env:ProgramFiles\Microsoft Visual Studio","${env:ProgramFiles(x86)}\Microsoft Visual Studio" `
        -Directory -ErrorAction SilentlyContinue |
        ForEach-Object { Get-ChildItem $_.FullName -Directory -ErrorAction SilentlyContinue } |
        ForEach-Object { $_.FullName } |
        Where-Object { Test-Path (Join-Path $_ "VC\Auxiliary\Build\vcvars64.bat") } |
        Select-Object -First 1
}
if (-not $vsPath) {
    throw "No Visual Studio C++ toolchain found. Install the 'Desktop development with C++' workload."
}

$vcvars = Join-Path $vsPath "VC\Auxiliary\Build\vcvars64.bat"
Write-Host "Using $vcvars"

cmd /c "call `"$vcvars`" >nul 2>&1 && cd /d `"$vendor`" && nmake /f makefile.vc"
if ($LASTEXITCODE -ne 0) { throw "nmake failed with exit code $LASTEXITCODE" }

$built = Join-Path $vendor "xRITDecompress\xRITDecompress.exe"
if (-not (Test-Path $built)) { throw "build reported success but $built is missing" }

Copy-Item $built $PSScriptRoot -Force
Write-Host "`nInstalled: $(Join-Path $PSScriptRoot 'xRITDecompress.exe')"

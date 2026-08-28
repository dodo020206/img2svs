$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $projectRoot

cargo fmt --all -- --check
cargo build --release

$binary = Join-Path $projectRoot "target\release\img2svs-rust.exe"
$vipsSource = Join-Path $projectRoot "..\img2svs-python\vips"
$vipsDestination = Join-Path $projectRoot "target\release\vips"
if (Test-Path -LiteralPath $vipsSource) {
    Copy-Item -LiteralPath $vipsSource -Destination $vipsDestination -Recurse -Force
    Write-Host "Copied OpenSlide/libvips runtime: $vipsDestination"
}
$ffmpegSource = Join-Path $projectRoot "..\img2svs-python\.venv-package\Lib\site-packages\av.libs"
$ffmpegDestination = Join-Path $projectRoot "target\release\av.libs"
if (Test-Path -LiteralPath $ffmpegSource) {
    Copy-Item -LiteralPath $ffmpegSource -Destination $ffmpegDestination -Recurse -Force
    Write-Host "Copied FFmpeg HEVC runtime: $ffmpegDestination"
}
Write-Host "Built: $binary"
Write-Host "Run:   $binary"

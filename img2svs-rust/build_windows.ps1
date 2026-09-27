<#
.SYNOPSIS
    Build img2svs-rust.

.DESCRIPTION
    Every input format is decoded and written by this repository, including
    HEVC-compressed .sdpc/.dyqx sources, so the build has no native runtime
    dependency at all and nothing has to be copied next to the executable.
#>

$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $projectRoot

cargo fmt --all -- --check
cargo build --release

$binary = Join-Path $projectRoot "target\release\img2svs-rust.exe"

Write-Host "Built: $binary"
Write-Host "Run:   $binary"

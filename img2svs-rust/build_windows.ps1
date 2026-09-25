<#
.SYNOPSIS
    Build img2svs-rust and place the native runtimes next to the executable.

.DESCRIPTION
    Runtime lookup order for each dependency (first match wins):

      1. -VipsHome / -FfmpegHome parameter
      2. VIPS_HOME / FFMPEG_HOME environment variable
      3. <repository root>\third_party\vips and \third_party\av.libs
      4. %USERPROFILE%\vips

    Nothing points into img2svs-python. Populate third_party\ once with
    scripts\fetch_native_runtimes.ps1 and both projects use the same copy.
#>
param(
    [string]$VipsHome,
    [string]$FfmpegHome
)

$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repositoryRoot = Split-Path -Parent $projectRoot
Set-Location $projectRoot

function Test-RuntimeRoot {
    param(
        [string]$Root,
        [string]$NamePattern
    )
    if (-not $Root) { return $false }
    if (-not (Test-Path -LiteralPath $Root)) { return $false }
    foreach ($subdirectory in @("bin", ".")) {
        $directory = if ($subdirectory -eq ".") { $Root } else { Join-Path $Root $subdirectory }
        if (-not (Test-Path -LiteralPath $directory)) { continue }
        if (Get-ChildItem -LiteralPath $directory -Filter $NamePattern -File -ErrorAction SilentlyContinue |
                Select-Object -First 1) {
            return $true
        }
    }
    return $false
}

function Find-RuntimeRoot {
    param(
        [string[]]$Candidates,
        [string]$NamePattern
    )
    foreach ($candidate in $Candidates) {
        if (Test-RuntimeRoot -Root $candidate -NamePattern $NamePattern) { return $candidate }
    }
    return $null
}

function Copy-RuntimeDirectory {
    param(
        [string]$Source,
        [string]$Destination,
        [string]$Label
    )
    $sourcePath = (Resolve-Path -LiteralPath $Source).Path.TrimEnd("\", "/")
    if (Test-Path -LiteralPath $Destination) {
        $destinationPath = (Resolve-Path -LiteralPath $Destination).Path.TrimEnd("\", "/")
        if ($destinationPath -eq $sourcePath) {
            Write-Host "$Label runtime already in place: $Destination"
            return
        }
        Remove-Item -LiteralPath $Destination -Recurse -Force
    }
    Copy-Item -LiteralPath $sourcePath -Destination $Destination -Recurse -Force
    Write-Host "Copied $Label runtime: $Destination"
}

cargo fmt --all -- --check
cargo build --release

$binary = Join-Path $projectRoot "target\release\img2svs-rust.exe"
$releaseRoot = Join-Path $projectRoot "target\release"

$vipsCandidates = @($VipsHome, $env:VIPS_HOME, (Join-Path $repositoryRoot "third_party\vips"))
if ($env:USERPROFILE) { $vipsCandidates += (Join-Path $env:USERPROFILE "vips") }
$vipsRoot = Find-RuntimeRoot -Candidates $vipsCandidates -NamePattern "libvips-*.dll"
if ($vipsRoot) {
    Copy-RuntimeDirectory -Source $vipsRoot -Destination (Join-Path $releaseRoot "vips") -Label "OpenSlide/libvips"
} else {
    Write-Warning "libvips runtime not found; .ndpi/.mrxs/.tif conversion stays unavailable."
    Write-Warning "Run scripts\fetch_native_runtimes.ps1, pass -VipsHome, or set VIPS_HOME."
}

$ffmpegCandidates = @($FfmpegHome, $env:FFMPEG_HOME, (Join-Path $repositoryRoot "third_party\av.libs"))
$avLibsRoot = Find-RuntimeRoot -Candidates $ffmpegCandidates -NamePattern "avcodec-*.dll"
if ($avLibsRoot) {
    Copy-RuntimeDirectory -Source $avLibsRoot -Destination (Join-Path $releaseRoot "av.libs") -Label "FFmpeg"
} else {
    Write-Warning "FFmpeg runtime not found; HEVC-compressed .sdpc/.dyqx stays unavailable."
    Write-Warning "Run scripts\fetch_native_runtimes.ps1, pass -FfmpegHome, or set FFMPEG_HOME."
}

Write-Host "Built: $binary"
Write-Host "Run:   $binary"

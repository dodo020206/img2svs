<#
.SYNOPSIS
    Package the Rust converter as two portable Windows builds: GUI and console CLI.

.DESCRIPTION
    Both variants come from the same source; the Console one is built with
    `--no-default-features`, which drops the optional `gui` feature (egui/rfd)
    and keeps the console subsystem so command-line output stays visible.

    Every input format is decoded by this repository, so the only native runtime
    a package needs is FFmpeg, and only for HEVC-compressed SDPC/DYQX sources:

      dist\PathologySVSConverter-rust-gui\   img2svs-rust.exe + av.libs\
      dist\PathologySVSConverter-rust-cli\   img2svs-cli.exe  + av.libs\

    Both directories are zipped and accompanied by a SHA256 checksum.

    Runtime lookup order, first match wins:
      1. -FfmpegHome
      2. FFMPEG_HOME environment variable
      3. <repository root>\third_party\av.libs

.PARAMETER FfmpegHome
    Directory that contains the FFmpeg DLLs (av.libs).

.PARAMETER OutputDirectory
    Directory that receives the two package folders and the ZIPs.
    Defaults to <repository root>\dist.

.EXAMPLE
    pwsh -File scripts\package_windows.ps1

.EXAMPLE
    pwsh -File scripts\package_windows.ps1 -FfmpegHome D:\av.libs
#>
[CmdletBinding()]
param(
    [string]$FfmpegHome,
    [string]$OutputDirectory
)

$ErrorActionPreference = "Stop"

$scriptDirectory = Split-Path -Parent $MyInvocation.MyCommand.Path
$repositoryRoot = Split-Path -Parent $scriptDirectory
$rustRoot = Join-Path $repositoryRoot "img2svs-rust"
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repositoryRoot "dist" }

function Resolve-Runtime {
    param(
        [string]$Explicit,
        [string]$EnvironmentVariable,
        [string[]]$SharedCandidates,
        [string]$Name
    )

    if ($Explicit) {
        if (-not (Test-Path -LiteralPath $Explicit)) { throw "$Name not found: $Explicit" }
        return (Resolve-Path -LiteralPath $Explicit).Path
    }
    if ($EnvironmentVariable) {
        $fromEnvironment = [Environment]::GetEnvironmentVariable($EnvironmentVariable)
        if ($fromEnvironment) {
            if (-not (Test-Path -LiteralPath $fromEnvironment)) {
                throw "$EnvironmentVariable points at a missing directory: $fromEnvironment"
            }
            return (Resolve-Path -LiteralPath $fromEnvironment).Path
        }
    }
    foreach ($candidate in $SharedCandidates) {
        if (Test-Path -LiteralPath $candidate) { return (Resolve-Path -LiteralPath $candidate).Path }
    }
    return $null
}

$ffmpeg = Resolve-Runtime -Explicit $FfmpegHome -EnvironmentVariable "FFMPEG_HOME" `
    -SharedCandidates @((Join-Path $repositoryRoot "third_party\av.libs")) -Name "FFmpeg runtime"

if (-not $ffmpeg) {
    Write-Warning "FFmpeg runtime not found; HEVC SDPC/DYQX will be unavailable in the packages."
} else {
    Write-Host "[ok  ] FFmpeg        -> $ffmpeg"
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null

# Each variant is built in turn and copied out immediately, because both share
# the same target\release\img2svs-rust.exe path.
$variants = @(
    @{ Name = "PathologySVSConverter-rust-gui"; Executable = "img2svs-rust.exe"; Arguments = @() },
    @{ Name = "PathologySVSConverter-rust-cli"; Executable = "img2svs-cli.exe"; Arguments = @("--no-default-features") }
)

Push-Location $rustRoot
try {
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw "cargo fmt --all -- --check failed" }

    foreach ($variant in $variants) {
        $packageRoot = Join-Path $OutputDirectory $variant.Name
        if (Test-Path -LiteralPath $packageRoot) {
            Remove-Item -LiteralPath $packageRoot -Recurse -Force
        }
        New-Item -ItemType Directory -Force -Path $packageRoot | Out-Null

        Write-Host "[build] $($variant.Name)$(if ($variant.Arguments.Count) { ' (' + ($variant.Arguments -join ' ') + ')' })"
        cargo build --release --locked @($variant.Arguments)
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $($variant.Name)" }

        Copy-Item -LiteralPath "target\release\img2svs-rust.exe" `
            -Destination (Join-Path $packageRoot $variant.Executable) -Force
        Copy-Item -LiteralPath "README.md" -Destination $packageRoot -Force

        if ($ffmpeg) {
            Copy-Item -LiteralPath $ffmpeg -Destination (Join-Path $packageRoot "av.libs") -Recurse -Force
        }

        $executablePath = Join-Path $packageRoot $variant.Executable
        $reported = (& $executablePath --version 2>&1 | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { throw "$($variant.Executable) failed to start" }
        if (-not $reported) {
            # A GUI-subsystem executable does not attach to the console, so its
            # --version output cannot be captured here; the exit code above is
            # the startup check.
            $reported = "$($variant.Executable) started (GUI subsystem, no console output)"
        }
        Write-Host "[ok  ] $reported"

        $zipPath = Join-Path $OutputDirectory "$($variant.Name).zip"
        if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
        Compress-Archive -LiteralPath $packageRoot -DestinationPath $zipPath -CompressionLevel Optimal
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $zipPath).Hash.ToLowerInvariant()
        "$hash  $($variant.Name).zip" | Set-Content -LiteralPath "$zipPath.sha256" -Encoding ascii

        $zipSize = [math]::Round((Get-Item -LiteralPath $zipPath).Length / 1MB, 1)
        Write-Host "[zip ] $zipPath ($zipSize MB)"
        Write-Host "[sum ] $hash"
    }
} finally {
    Pop-Location
}

Write-Host ""
Write-Host "Packages written to $OutputDirectory"
foreach ($variant in $variants) {
    $zipPath = Join-Path $OutputDirectory "$($variant.Name).zip"
    $size = [math]::Round((Get-Item -LiteralPath $zipPath).Length / 1MB, 1)
    Write-Host ("  {0,-34} {1,8} MB" -f "$($variant.Name).zip", $size)
}

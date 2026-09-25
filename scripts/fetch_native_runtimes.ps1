<#
.SYNOPSIS
    Download the native runtimes shared by img2svs-rust and img2svs-python.

.DESCRIPTION
    libvips (with its OpenSlide module) and the FFmpeg runtime (the av.libs
    directory shipped inside the PyAV wheel) are placed in
    <repository root>\third_party.

    That directory is not tracked by git, so a fresh clone must run this script
    once before NDPI/MRXS support (libvips) or HEVC-compressed SDPC/DYQX support
    (FFmpeg) becomes available.

    Both projects read the runtimes from this single location, so nothing is
    duplicated inside the repository.

    The script is idempotent: an already installed runtime is skipped unless
    -Force is given. Versions default to LIBVIPS_VERSION / PYAV_VERSION and fall
    back to the pins used by the Windows CI workflow.

.PARAMETER Destination
    Directory that receives vips\ and av.libs\. Defaults to third_party\ in the
    repository root.

.PARAMETER Force
    Re-download and replace runtimes that are already present.

.EXAMPLE
    pwsh -File scripts\fetch_native_runtimes.ps1

.EXAMPLE
    pwsh -File scripts\fetch_native_runtimes.ps1 -Force

.EXAMPLE
    pwsh -File scripts\fetch_native_runtimes.ps1 -Destination D:\img2svs-runtimes
#>
[CmdletBinding()]
param(
    [string]$Destination,
    [string]$LibvipsVersion = $env:LIBVIPS_VERSION,
    [string]$PyAvVersion = $env:PYAV_VERSION,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

if (-not $LibvipsVersion) { $LibvipsVersion = "8.18.1" }
if (-not $PyAvVersion) { $PyAvVersion = "18.1.0" }

$scriptDirectory = Split-Path -Parent $MyInvocation.MyCommand.Path
$repositoryRoot = Split-Path -Parent $scriptDirectory
if (-not $Destination) { $Destination = Join-Path $repositoryRoot "third_party" }

$vipsTarget = Join-Path $Destination "vips"
$avLibsTarget = Join-Path $Destination "av.libs"
$downloadDirectory = Join-Path $Destination ".runtime-download"

New-Item -ItemType Directory -Force -Path $Destination, $downloadDirectory | Out-Null

# Windows PowerShell 5.1 still negotiates TLS 1.0 by default and aborts on
# GitHub. PowerShell 7 ignores this assignment.
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {
    Write-Verbose "Could not adjust the TLS protocol; using the platform default."
}

function Get-FirstMatchingFile {
    param(
        [string]$Directory,
        [string]$Filter
    )
    if (-not $Directory) { return $null }
    if (-not (Test-Path -LiteralPath $Directory)) { return $null }
    return Get-ChildItem -LiteralPath $Directory -Filter $Filter -File -ErrorAction SilentlyContinue |
        Select-Object -First 1
}

function Test-VipsRuntime {
    param([string]$Root)
    if (-not $Root) { return $false }
    foreach ($subdirectory in @("bin", ".")) {
        $directory = if ($subdirectory -eq ".") { $Root } else { Join-Path $Root $subdirectory }
        if (Get-FirstMatchingFile -Directory $directory -Filter "libvips-*.dll") { return $true }
    }
    return $false
}

function Test-AvLibsRuntime {
    param([string]$Root)
    if (-not $Root) { return $false }
    return $null -ne (Get-FirstMatchingFile -Directory $Root -Filter "avcodec-*.dll")
}

function Install-LibvipsRuntime {
    if ((Test-VipsRuntime -Root $vipsTarget) -and -not $Force) {
        Write-Host "[skip] libvips already present: $vipsTarget"
        return
    }

    $url = "https://github.com/libvips/build-win64-mxe/releases/download/v$LibvipsVersion/vips-dev-w64-all-$LibvipsVersion.zip"
    $archive = Join-Path $downloadDirectory "libvips-$LibvipsVersion.zip"
    $extractDirectory = Join-Path $downloadDirectory "libvips-extract"

    Write-Host "[get ] $url"
    Invoke-WebRequest -Uri $url -OutFile $archive

    if (Test-Path -LiteralPath $extractDirectory) {
        Remove-Item -LiteralPath $extractDirectory -Recurse -Force
    }
    Expand-Archive -LiteralPath $archive -DestinationPath $extractDirectory -Force

    $vipsExecutable = Get-ChildItem -LiteralPath $extractDirectory -Recurse -File -Filter "vips.exe" |
        Where-Object { $_.Directory.Name -eq "bin" } |
        Select-Object -First 1
    if (-not $vipsExecutable) {
        throw "The downloaded libvips archive does not contain bin\vips.exe"
    }
    $sourceRoot = Split-Path -Parent $vipsExecutable.Directory.FullName

    if (Test-Path -LiteralPath $vipsTarget) {
        Remove-Item -LiteralPath $vipsTarget -Recurse -Force
    }
    Copy-Item -LiteralPath $sourceRoot -Destination $vipsTarget -Recurse -Force
    Write-Host "[ok  ] libvips $LibvipsVersion -> $vipsTarget"
}

function Install-AvLibsRuntime {
    if ((Test-AvLibsRuntime -Root $avLibsTarget) -and -not $Force) {
        Write-Host "[skip] FFmpeg runtime already present: $avLibsTarget"
        return
    }

    $pythonCommand = Get-Command python -ErrorAction SilentlyContinue
    if (-not $pythonCommand) {
        $pythonCommand = Get-Command python3 -ErrorAction SilentlyContinue
    }
    if (-not $pythonCommand) {
        throw "Unpacking the FFmpeg runtime needs Python, but no 'python' or 'python3' executable was found on PATH."
    }

    $wheelDirectory = Join-Path $downloadDirectory "pyav"
    if (Test-Path -LiteralPath $wheelDirectory) {
        Remove-Item -LiteralPath $wheelDirectory -Recurse -Force
    }

    Write-Host "[get ] av==$PyAvVersion (PyAV wheel)"
    & $pythonCommand.Source -m pip install --disable-pip-version-check --only-binary=:all: --target $wheelDirectory "av==$PyAvVersion"
    if ($LASTEXITCODE -ne 0) {
        throw "Installing the PyAV wheel failed with exit code $LASTEXITCODE"
    }

    $avLibs = Get-ChildItem -LiteralPath $wheelDirectory -Directory -Filter "av.libs" -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $avLibs) {
        throw "The PyAV wheel does not contain an av.libs directory"
    }

    if (Test-Path -LiteralPath $avLibsTarget) {
        Remove-Item -LiteralPath $avLibsTarget -Recurse -Force
    }
    Copy-Item -LiteralPath $avLibs.FullName -Destination $avLibsTarget -Recurse -Force
    Write-Host "[ok  ] FFmpeg runtime $PyAvVersion -> $avLibsTarget"
}

Write-Host "Native runtimes -> $Destination"
Install-LibvipsRuntime
Install-AvLibsRuntime

# Only the installed runtimes are kept; the extracted trees and the wheels
# would otherwise double the footprint of third_party\.
if (Test-Path -LiteralPath $downloadDirectory) {
    Remove-Item -LiteralPath $downloadDirectory -Recurse -Force
}

Write-Host ""
Write-Host "Done. Consumers pick these up automatically:"
Write-Host "  img2svs-rust\build_windows.ps1            copies both next to the executable"
Write-Host "  img2svs-python\build_windows_exe.bat      uses third_party\vips as VIPS_HOME"
Write-Host "  .github\workflows\build-rust-windows.yml  packages both into the release ZIP"
Write-Host "Point elsewhere with -Destination, VIPS_HOME or FFMPEG_HOME."

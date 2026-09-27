<#
.SYNOPSIS
    Package the Rust converter as two portable Windows builds: GUI and console CLI.

.DESCRIPTION
    Both variants come from the same source; the Console one is built with
    `--no-default-features`, which drops the optional `gui` feature (egui/rfd)
    and keeps the console subsystem so command-line output stays visible.

    Every input format, including HEVC-compressed SDPC/DYQX, is decoded by this
    repository, so a package is just the executable plus its README:

      dist\PathologySVSConverter-rust-gui\   img2svs-rust.exe
      dist\PathologySVSConverter-rust-cli\   img2svs-cli.exe

    Both directories are zipped and accompanied by a SHA256 checksum.

.PARAMETER OutputDirectory
    Directory that receives the two package folders and the ZIPs.
    Defaults to <repository root>\dist.

.EXAMPLE
    pwsh -File scripts\package_windows.ps1
#>
[CmdletBinding()]
param(
    [string]$OutputDirectory
)

$ErrorActionPreference = "Stop"

$scriptDirectory = Split-Path -Parent $MyInvocation.MyCommand.Path
$repositoryRoot = Split-Path -Parent $scriptDirectory
$rustRoot = Join-Path $repositoryRoot "img2svs-rust"
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repositoryRoot "dist" }

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

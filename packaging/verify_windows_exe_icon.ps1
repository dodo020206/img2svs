param(
    [Parameter(Mandatory = $true)]
    [string]$ExePath,

    [Parameter(Mandatory = $true)]
    [string]$ExpectedIconPath,

    [Parameter(Mandatory = $true)]
    [string]$PyInstallerDefaultIconPath
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

function Get-IconPixels {
    param(
        [Parameter(Mandatory = $true)]
        [string]$IconSource,

        [Parameter(Mandatory = $true)]
        [bool]$IsExecutable
    )

    if ($IsExecutable) {
        $probePath = Join-Path ([System.IO.Path]::GetTempPath()) (
            "img2svs-icon-probe-" + [Guid]::NewGuid().ToString("N") + ".exe"
        )
        Copy-Item -LiteralPath $IconSource -Destination $probePath
        $icon = [System.Drawing.Icon]::ExtractAssociatedIcon($probePath)
    }
    else {
        $probePath = $null
        $icon = New-Object Drawing.Icon($IconSource, 32, 32)
    }
    if ($null -eq $icon) {
        throw "Unable to load icon: $IconSource"
    }

    try {
        $bitmap = $icon.ToBitmap()
        try {
            $pixels = [int[]]::new($bitmap.Width * $bitmap.Height)
            $offset = 0
            for ($y = 0; $y -lt $bitmap.Height; $y++) {
                for ($x = 0; $x -lt $bitmap.Width; $x++) {
                    $pixels[$offset] = $bitmap.GetPixel($x, $y).ToArgb()
                    $offset++
                }
            }
            return $pixels
        }
        finally {
            $bitmap.Dispose()
        }
    }
    finally {
        $icon.Dispose()
        if ($probePath) {
            Remove-Item -LiteralPath $probePath -Force -ErrorAction SilentlyContinue
        }
    }
}

function Get-IconDistance {
    param(
        [Parameter(Mandatory = $true)]
        [int[]]$Left,

        [Parameter(Mandatory = $true)]
        [int[]]$Right
    )

    if ($Left.Length -ne $Right.Length) {
        throw "Icon dimensions do not match"
    }

    [long]$distance = 0
    for ($index = 0; $index -lt $Left.Length; $index++) {
        $leftColor = [System.Drawing.Color]::FromArgb($Left[$index])
        $rightColor = [System.Drawing.Color]::FromArgb($Right[$index])
        $distance += [Math]::Abs([int]$leftColor.A - [int]$rightColor.A)
        $distance += [Math]::Abs([int]$leftColor.R - [int]$rightColor.R)
        $distance += [Math]::Abs([int]$leftColor.G - [int]$rightColor.G)
        $distance += [Math]::Abs([int]$leftColor.B - [int]$rightColor.B)
    }
    return $distance
}

$resolvedExe = (Resolve-Path -LiteralPath $ExePath).Path
$resolvedExpected = (Resolve-Path -LiteralPath $ExpectedIconPath).Path
$resolvedDefault = (Resolve-Path -LiteralPath $PyInstallerDefaultIconPath).Path

$actualPixels = Get-IconPixels -IconSource $resolvedExe -IsExecutable $true
$expectedPixels = Get-IconPixels -IconSource $resolvedExpected -IsExecutable $false
$defaultPixels = Get-IconPixels -IconSource $resolvedDefault -IsExecutable $false
$expectedDistance = Get-IconDistance -Left $actualPixels -Right $expectedPixels
$defaultDistance = Get-IconDistance -Left $actualPixels -Right $defaultPixels

Write-Host "[INFO] EXE icon distances: expected=$expectedDistance default=$defaultDistance"

if ($defaultDistance -eq 0) {
    throw "EXE icon verification failed: packaged icon is the PyInstaller default icon"
}
if ($PSVersionTable.PSEdition -eq "Core" -and $expectedDistance -ne 0) {
    throw "EXE icon verification failed: packaged icon does not match the expected app icon"
}

Write-Host "[OK] EXE icon verified: expected_distance=$expectedDistance default_distance=$defaultDistance"

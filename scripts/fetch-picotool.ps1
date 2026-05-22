# SPDX-License-Identifier: GPL-3.0-or-later
#
# Downloads picotool.exe into internal/picotool/binaries/ so a tagged
# build (`go build -tags embed_picotool`) can embed it. Default builds
# don't need this -- they fall back to the host PATH.
#
# Idempotent: skips the download if the target already exists. Pass
# -Force to re-fetch.

param(
    [string]$Version = "2.2.0-3",
    [string]$AssetName = "picotool-2.2.0-a4-x64-win.zip",
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$repoRoot   = Split-Path -Parent $PSScriptRoot
$binariesDir = Join-Path $repoRoot 'internal\picotool\binaries'
$outExe     = Join-Path $binariesDir 'picotool.exe'

if ((Test-Path -LiteralPath $outExe) -and (-not $Force)) {
    Write-Host "picotool already present at $outExe (pass -Force to re-fetch)" -ForegroundColor Yellow
    exit 0
}

New-Item -ItemType Directory -Force -Path $binariesDir | Out-Null

$url = "https://github.com/raspberrypi/pico-sdk-tools/releases/download/v$Version/$AssetName"
Write-Host "fetching $url" -ForegroundColor Cyan

# TLS 1.2 for Windows PowerShell 5.1.
[Net.ServicePointManager]::SecurityProtocol = `
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$tmpZip = Join-Path $binariesDir $AssetName
try {
    Invoke-WebRequest -Uri $url -OutFile $tmpZip -UseBasicParsing
} catch {
    throw "download failed: $($_.Exception.Message)"
}

Expand-Archive -LiteralPath $tmpZip -DestinationPath $binariesDir -Force
Remove-Item -LiteralPath $tmpZip -Force

# Some sdk-tools releases nest picotool.exe inside a versioned subfolder;
# find it and lift it to the canonical location.
if (-not (Test-Path -LiteralPath $outExe)) {
    $found = Get-ChildItem -LiteralPath $binariesDir -Recurse -File -Filter picotool.exe | Select-Object -First 1
    if ($null -eq $found) {
        throw "picotool.exe not found in extracted archive at $binariesDir"
    }
    Copy-Item -LiteralPath $found.FullName -Destination $outExe -Force
}

$size = (Get-Item -LiteralPath $outExe).Length
Write-Host "[OK] picotool.exe ready at $outExe ($([math]::Round($size / 1MB, 2)) MB)" -ForegroundColor Green

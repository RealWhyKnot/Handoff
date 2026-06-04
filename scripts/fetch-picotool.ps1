# SPDX-License-Identifier: GPL-3.0-or-later
#
# Downloads picotool.exe into internal/picotool/binaries/ so a tagged
# build (`go build -tags embed_picotool`) can embed it. Default builds
# don't need this -- they fall back to the host PATH.
#
# Resolution order:
#   1. -AssetUrl <url>   -- direct download URL (skips API lookup).
#   2. -Version <ver>    -- pin to that pico-sdk-tools release tag.
#   3. Latest release on raspberrypi/pico-sdk-tools (the default).
#
# Idempotent: skips the download if the target already exists. Pass
# -Force to re-fetch.

param(
    [string]$Version = "",     # e.g. "2.2.0-3" to pin; empty = latest
    [string]$AssetUrl = "",    # direct URL override
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$binariesDir = Join-Path $repoRoot 'internal\picotool\binaries'
$outExe = Join-Path $binariesDir 'picotool.exe'

if ((Test-Path -LiteralPath $outExe) -and (-not $Force)) {
    Write-Host "picotool already present at $outExe (pass -Force to re-fetch)" -ForegroundColor Yellow
    exit 0
}

New-Item -ItemType Directory -Force -Path $binariesDir | Out-Null

# TLS 1.2 for Windows PowerShell 5.1.
[Net.ServicePointManager]::SecurityProtocol = `
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

# Resolve the asset URL. Priority: explicit -AssetUrl > -Version pin > latest.
function Resolve-PicotoolAssetUrl {
    param([string]$RequestedVersion)

    $api = if ($RequestedVersion) {
        # Tag URL pattern: tags on this repo are 'v2.2.0-3' shape.
        $tag = if ($RequestedVersion.StartsWith('v')) { $RequestedVersion } else { "v$RequestedVersion" }
        "https://api.github.com/repos/raspberrypi/pico-sdk-tools/releases/tags/$tag"
    }
    else {
        "https://api.github.com/repos/raspberrypi/pico-sdk-tools/releases/latest"
    }

    $headers = @{ 'User-Agent' = 'Handoff-fetch-picotool' }
    if ($env:GITHUB_TOKEN) { $headers['Authorization'] = "Bearer $env:GITHUB_TOKEN" }

    Write-Host "querying $api" -ForegroundColor Cyan
    try {
        $release = Invoke-RestMethod -Uri $api -Headers $headers -UseBasicParsing
    }
    catch {
        throw "GitHub API query failed: $($_.Exception.Message)"
    }

    # Find the x64 Windows picotool asset. Real-world names follow the
    # 'picotool-<ver>-x64-win.zip' shape; match that to ignore the Linux
    # / macOS / aarch64 assets that ship in the same release.
    $asset = $release.assets | Where-Object {
        $_.name -match '^picotool-.*-x64-win\.zip$'
    } | Select-Object -First 1

    if ($null -eq $asset) {
        $names = ($release.assets | ForEach-Object { $_.name }) -join ', '
        throw "no x64-win picotool asset in release $($release.tag_name); available assets: $names"
    }

    Write-Host "release : $($release.tag_name)" -ForegroundColor Gray
    Write-Host "asset   : $($asset.name) ($([math]::Round($asset.size / 1MB, 2)) MB)" -ForegroundColor Gray
    return $asset.browser_download_url
}

if ($AssetUrl) {
    $url = $AssetUrl
    Write-Host "using explicit asset url" -ForegroundColor Cyan
}
else {
    $url = Resolve-PicotoolAssetUrl -RequestedVersion $Version
}

Write-Host "fetching $url" -ForegroundColor Cyan
$tmpZip = Join-Path $binariesDir 'picotool-download.zip'
try {
    Invoke-WebRequest -Uri $url -OutFile $tmpZip -UseBasicParsing
}
catch {
    throw "download failed: $($_.Exception.Message)"
}

Expand-Archive -LiteralPath $tmpZip -DestinationPath $binariesDir -Force
Remove-Item -LiteralPath $tmpZip -Force

# Some sdk-tools releases nest picotool.exe inside a versioned subfolder;
# find it and lift it to the canonical location. Then prune the nested dir.
if (-not (Test-Path -LiteralPath $outExe)) {
    $found = Get-ChildItem -LiteralPath $binariesDir -Recurse -File -Filter picotool.exe |
        Select-Object -First 1
    if ($null -eq $found) {
        throw "picotool.exe not found in extracted archive at $binariesDir"
    }
    Copy-Item -LiteralPath $found.FullName -Destination $outExe -Force
}

# Sweep any unpacked subdirectories so the binaries/ folder only ever
# holds picotool.exe + .keep. Otherwise repeated runs leave stale dirs.
Get-ChildItem -LiteralPath $binariesDir -Directory -ErrorAction SilentlyContinue |
    ForEach-Object { Remove-Item -LiteralPath $_.FullName -Recurse -Force }

$size = (Get-Item -LiteralPath $outExe).Length
Write-Host "[OK] picotool.exe ready at $outExe ($([math]::Round($size / 1MB, 2)) MB)" -ForegroundColor Green

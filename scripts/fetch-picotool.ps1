#!/usr/bin/env pwsh
[CmdletBinding()]
param(
    [string] $Version = '',
    [string] $AssetUrl = '',
    [switch] $Force
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Resolve-PicotoolAssetUrl {
    [CmdletBinding()]
    [OutputType([string])]
    param([string] $RequestedVersion)

    $api = 'https://api.github.com/repos/raspberrypi/pico-sdk-tools/releases/latest'
    if ($RequestedVersion) {
        $tag = $RequestedVersion
        if (-not $tag.StartsWith('v')) { $tag = "v$tag" }
        $api = "https://api.github.com/repos/raspberrypi/pico-sdk-tools/releases/tags/$tag"
    }

    $headers = @{ 'User-Agent' = 'Handoff-fetch-picotool' }
    if ($env:GITHUB_TOKEN) { $headers['Authorization'] = "Bearer $env:GITHUB_TOKEN" }

    Write-Host "querying $api" -ForegroundColor Cyan
    $release = Invoke-RestMethod -Uri $api -Headers $headers -UseBasicParsing

    $assets = @($release.assets)
    $asset = $assets | Where-Object { $_.name -match '^picotool-.*-x64-win\.zip$' } | Select-Object -First 1
    if ($null -eq $asset) {
        $names = @(foreach ($item in $assets) { $item.name }) -join ', '
        throw "no x64-win picotool asset in release $($release.tag_name); available assets: $names"
    }

    Write-Host "release : $($release.tag_name)" -ForegroundColor Gray
    Write-Host "asset   : $($asset.name) ($([math]::Round($asset.size / 1MB, 2)) MB)" -ForegroundColor Gray
    $asset.browser_download_url
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$binariesDir = Join-Path $repoRoot 'assets\picotool'
$outExe = Join-Path $binariesDir 'picotool.exe'

if ((Test-Path -LiteralPath $outExe) -and (-not $Force)) {
    Write-Host "picotool already present at $outExe (pass -Force to re-fetch)" -ForegroundColor Yellow
    exit 0
}

$null = [IO.Directory]::CreateDirectory($binariesDir)

$tls = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
[Net.ServicePointManager]::SecurityProtocol = $tls

if ($AssetUrl) {
    $url = $AssetUrl
    Write-Host 'using explicit asset url' -ForegroundColor Cyan
} else {
    $url = Resolve-PicotoolAssetUrl -RequestedVersion $Version
}

Write-Host "fetching $url" -ForegroundColor Cyan
$tmpZip = Join-Path $binariesDir 'picotool-download.zip'
Invoke-WebRequest -Uri $url -OutFile $tmpZip -UseBasicParsing

Expand-Archive -LiteralPath $tmpZip -DestinationPath $binariesDir -Force
Remove-Item -LiteralPath $tmpZip -Force

if (-not (Test-Path -LiteralPath $outExe)) {
    $found = Get-ChildItem -LiteralPath $binariesDir -Recurse -File -Filter picotool.exe |
        Select-Object -First 1
    if ($null -eq $found) {
        throw "picotool.exe not found in extracted archive at $binariesDir"
    }
    Copy-Item -LiteralPath $found.FullName -Destination $outExe -Force
}

foreach ($dir in @(Get-ChildItem -LiteralPath $binariesDir -Directory)) {
    Remove-Item -LiteralPath $dir.FullName -Recurse -Force
}

$size = (Get-Item -LiteralPath $outExe).Length
Write-Host "[OK] picotool.exe ready at $outExe ($([math]::Round($size / 1MB, 2)) MB)" -ForegroundColor Green
exit 0

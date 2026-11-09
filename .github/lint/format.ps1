#!/usr/bin/env pwsh
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
Set-Location -LiteralPath $repoRoot

git config --local core.hooksPath .githooks
if ($LASTEXITCODE -ne 0) { throw "git config failed with exit code $LASTEXITCODE" }

cargo fmt
if ($LASTEXITCODE -ne 0) { throw "cargo fmt failed with exit code $LASTEXITCODE" }

if (-not (Get-Module -ListAvailable -Name PSScriptAnalyzer)) {
    Write-Warning 'PSScriptAnalyzer is not installed; skipped PowerShell formatting.'
    exit 0
}

Import-Module PSScriptAnalyzer
$settings = Join-Path $PSScriptRoot 'PSScriptAnalyzerSettings.psd1'
$files = @(git ls-files -- '*.ps1' '*.psm1' '*.psd1')
if ($LASTEXITCODE -ne 0) { throw "git ls-files failed with exit code $LASTEXITCODE" }

foreach ($file in $files) {
    $path = Join-Path $repoRoot $file
    $text = [IO.File]::ReadAllText($path) -replace "`r`n?", "`n"
    $formatted = Invoke-Formatter -ScriptDefinition $text -Settings $settings
    if ($formatted -ne $text) {
        [IO.File]::WriteAllText($path, $formatted)
        Write-Host "Formatted $file"
    }
}
exit 0

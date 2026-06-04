#!/usr/bin/env pwsh
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $RepoRoot

& git config --local core.hooksPath .githooks

$goFiles = & git ls-files '*.go'
if ($goFiles) {
    & gofmt -w @goFiles
    if ($LASTEXITCODE -ne 0) { throw "gofmt failed with exit code $LASTEXITCODE" }
}

if (Get-Module -ListAvailable -Name PSScriptAnalyzer) {
    Import-Module PSScriptAnalyzer
    $psFiles = & git ls-files '*.ps1' '*.psm1' '*.psd1'
    foreach ($file in $psFiles) {
        $path = Join-Path $RepoRoot $file
        $text = [System.IO.File]::ReadAllText($path) -replace "`r`n?", "`n"
        $formatted = Invoke-Formatter -ScriptDefinition $text
        if ($formatted -ne $text) {
            [System.IO.File]::WriteAllText($path, $formatted, [System.Text.UTF8Encoding]::new($false))
            Write-Host "Formatted $file"
        }
    }
} else {
    Write-Warning 'PSScriptAnalyzer is not installed; skipped PowerShell formatting.'
}

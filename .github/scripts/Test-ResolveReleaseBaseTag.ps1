#!/usr/bin/env pwsh
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$resolver = Join-Path $PSScriptRoot 'Resolve-ReleaseBaseTag.ps1'
$repo = Join-Path ([System.IO.Path]::GetTempPath()) ("release-base-test-" + [System.Guid]::NewGuid().ToString('N'))

function Invoke-Git {
    param(
        [Parameter(Mandatory = $true)]
        [string[]] $Arguments
    )

    & git @Arguments | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "git $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

function Add-TestCommit {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Content,
        [Parameter(Mandatory = $true)]
        [string] $Subject
    )

    Set-Content -LiteralPath (Join-Path $repo 'sample.txt') -Value $Content -Encoding UTF8
    Invoke-Git @('add', 'sample.txt')
    Invoke-Git @('commit', '-m', $Subject)
}

try {
    New-Item -ItemType Directory -Path $repo -Force | Out-Null
    Push-Location $repo

    Invoke-Git @('init', '-q')
    Invoke-Git @('config', 'user.email', 'release-test@example.com')
    Invoke-Git @('config', 'user.name', 'Release Test')

    Add-TestCommit -Content 'base' -Subject 'chore: base'
    Invoke-Git @('tag', 'v2026.5.1.0')

    Add-TestCommit -Content 'pre' -Subject 'feat: prerelease patch'
    Invoke-Git @('tag', 'v2026.5.2.0-beta')

    Add-TestCommit -Content 'stable' -Subject 'fix: stable patch'
    Invoke-Git @('tag', 'v2026.5.3.0')

    $stableBase = & $resolver -Tag 'v2026.5.3.0'
    if ($stableBase -ne 'v2026.5.1.0') {
        throw "Stable base was '$stableBase', expected 'v2026.5.1.0'."
    }

    $preBase = & $resolver -Tag 'v2026.5.2.0-beta'
    if ($preBase -ne 'v2026.5.1.0') {
        throw "Prerelease base was '$preBase', expected 'v2026.5.1.0'."
    }

    Write-Host 'Resolve-ReleaseBaseTag tests passed.'
}
finally {
    Pop-Location
    Remove-Item -LiteralPath $repo -Recurse -Force
}

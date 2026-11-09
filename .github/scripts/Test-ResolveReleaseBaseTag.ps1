#!/usr/bin/env pwsh
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Git {
    [CmdletBinding()]
    [OutputType([void])]
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments
    )

    $null = git @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "git $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

function Add-TestCommit {
    [CmdletBinding()]
    [OutputType([void])]
    param(
        [Parameter(Mandatory)]
        [string] $RepoRoot,
        [Parameter(Mandatory)]
        [string] $Content,
        [Parameter(Mandatory)]
        [string] $Subject
    )

    [IO.File]::WriteAllText((Join-Path $RepoRoot 'sample.txt'), "$Content`n")
    Invoke-Git -Arguments @('add', 'sample.txt')
    Invoke-Git -Arguments @('commit', '-m', $Subject)
}

function Assert-Equal {
    [CmdletBinding()]
    [OutputType([void])]
    param(
        [object] $Actual,
        [object] $Expected,
        [Parameter(Mandatory)]
        [string] $Message
    )

    if ($Actual -ne $Expected) {
        $script:failures++
        Write-Host "FAIL: $Message was '$Actual', expected '$Expected'."
    }
}

$script:failures = 0
$resolver = Join-Path $PSScriptRoot 'Resolve-ReleaseBaseTag.ps1'
$repo = Join-Path ([IO.Path]::GetTempPath()) ('release-base-test-' + [guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($repo)
Push-Location -LiteralPath $repo
try {
    Invoke-Git -Arguments @('init', '-q')
    Invoke-Git -Arguments @('config', 'user.email', 'release-test@example.com')
    Invoke-Git -Arguments @('config', 'user.name', 'Release Test')

    Add-TestCommit -RepoRoot $repo -Content 'base' -Subject 'chore: base'
    Invoke-Git -Arguments @('tag', 'v2026.5.1.0')

    Add-TestCommit -RepoRoot $repo -Content 'pre' -Subject 'feat: prerelease patch'
    Invoke-Git -Arguments @('tag', 'v2026.5.2.0-beta')

    Add-TestCommit -RepoRoot $repo -Content 'stable' -Subject 'fix: stable patch'
    Invoke-Git -Arguments @('tag', 'v2026.5.3.0')

    $stableBase = & $resolver -Tag 'v2026.5.3.0'
    if ($LASTEXITCODE -ne 0) { throw "The resolver failed with exit code $LASTEXITCODE" }
    Assert-Equal -Actual $stableBase -Expected 'v2026.5.1.0' -Message 'Stable base'

    $preBase = & $resolver -Tag 'v2026.5.2.0-beta'
    if ($LASTEXITCODE -ne 0) { throw "The resolver failed with exit code $LASTEXITCODE" }
    Assert-Equal -Actual $preBase -Expected 'v2026.5.1.0' -Message 'Prerelease base'
} finally {
    Pop-Location
    Remove-Item -LiteralPath $repo -Recurse -Force
}

if ($script:failures -gt 0) {
    Write-Host "$script:failures Resolve-ReleaseBaseTag checks failed."
    exit 1
}
Write-Host 'Resolve-ReleaseBaseTag tests passed.'
exit 0

#!/usr/bin/env pwsh
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$script = Join-Path $PSScriptRoot 'New-ReleaseNotes.ps1'
$templates = Join-Path $PSScriptRoot '..\release-template'
$repo = Join-Path ([System.IO.Path]::GetTempPath()) ("release-notes-test-" + [System.Guid]::NewGuid().ToString('N'))

function Invoke-Git([string[]] $Arguments) {
    & git @Arguments | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "git $($Arguments -join ' ') failed with exit code $LASTEXITCODE" }
}

function Add-TestCommit([string] $Subject) {
    Add-Content -LiteralPath (Join-Path $repo 'sample.txt') -Value $Subject -Encoding UTF8
    Invoke-Git @('add', 'sample.txt')
    Invoke-Git @('commit', '-q', '-m', $Subject)
}

function Assert-Contains([string] $Body, [string] $Expected) {
    if (-not $Body.Contains($Expected)) { throw "Expected release body to contain: $Expected`n---`n$Body" }
}

try {
    New-Item -ItemType Directory -Path $repo -Force | Out-Null
    Push-Location $repo
    Invoke-Git @('init', '-q')
    Invoke-Git @('config', 'user.email', 'release-test@example.com')
    Invoke-Git @('config', 'user.name', 'WhyKnot')
    Invoke-Git @('config', 'commit.gpgsign', 'false')

    Add-TestCommit 'feat(cli): first thing (2026.1.1.0-ABCD)'
    Invoke-Git @('tag', 'v2026.1.1.0')
    Add-TestCommit 'fix(exec): second thing (2026.1.2.0-BEEF)'
    Add-TestCommit 'plain subject with no prefix'
    Add-TestCommit 'feat: unscoped feature'
    Add-TestCommit 'ci(release): hidden [skip changelog]'
    Invoke-Git @('tag', 'v2026.1.2.0')

    $sha = (git rev-parse 'v2026.1.2.0^{}').Trim().Substring(0, 12)
    $body = (& $script -Tag 'v2026.1.2.0' -Repo 'RealWhyKnot/Handoff' -ExeSha256 'ABC123' -TemplateDir $templates) -join "`n"

    Assert-Contains $body '# Handoff v2026.1.2.0'
    Assert-Contains $body "## What's Changed"
    Assert-Contains $body "### Features`n- feat: unscoped feature by @RealWhyKnot in "
    Assert-Contains $body "### Bug Fixes`n- fix(exec): second thing by @RealWhyKnot in "
    Assert-Contains $body "### Other Changes`n- plain subject with no prefix by @RealWhyKnot in "
    Assert-Contains $body '**Full Changelog**: https://github.com/RealWhyKnot/Handoff/compare/v2026.1.1.0...v2026.1.2.0'
    Assert-Contains $body 'SHA256 of `handoff.exe`: `abc123`'
    Assert-Contains $body "- **Source:** commit ``$sha`` on ``main``"
    Assert-Contains $body '## Install (fresh)'
    Assert-Contains $body '## Uninstall'
    Assert-Contains $body '## What you need to do'
    if ($body -match '\(2026\.1\.2\.0-BEEF\)') { throw 'Build suffix leaked into release body' }
    if ($body -match 'first thing') { throw 'Commit before the previous tag leaked into release body' }
    if ($body -match 'hidden') { throw 'Commit marked skip changelog leaked into release body' }
    if ($body.IndexOf('### Features') -gt $body.IndexOf('### Bug Fixes')) { throw 'Features must come before Bug Fixes' }

    $first = (& $script -Tag 'v2026.1.1.0' -Repo 'RealWhyKnot/Handoff' -TemplateDir $templates) -join "`n"
    Assert-Contains $first '- feat(cli): first thing by @RealWhyKnot in '
    if ($first -match 'Full Changelog') { throw 'First release must not link a compare range' }
    if ($first -match 'File integrity') { throw 'File integrity section must be omitted without a hash' }

    Write-Host 'New-ReleaseNotes.ps1 tests passed.'
}
finally {
    Pop-Location
    if (Test-Path -LiteralPath $repo) { Remove-Item -LiteralPath $repo -Recurse -Force }
}

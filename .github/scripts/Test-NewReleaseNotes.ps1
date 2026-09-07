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

function Assert-Contains([string] $Body, [string] $Expected) {
    if (-not $Body.Contains($Expected)) { throw "Expected release body to contain: $Expected`n---`n$Body" }
}

function New-Changelog([string] $Text) {
    $path = Join-Path $repo ("changelog-" + [System.Guid]::NewGuid().ToString('N') + ".md")
    [System.IO.File]::WriteAllText($path, $Text, (New-Object System.Text.UTF8Encoding($false)))
    return $path
}

try {
    New-Item -ItemType Directory -Path $repo -Force | Out-Null
    Push-Location $repo
    Invoke-Git @('init', '-q')
    Invoke-Git @('config', 'user.email', 'release-test@example.com')
    Invoke-Git @('config', 'user.name', 'WhyKnot')
    Invoke-Git @('config', 'commit.gpgsign', 'false')
    Add-Content -LiteralPath (Join-Path $repo 'sample.txt') -Value 'sample' -Encoding UTF8
    Invoke-Git @('add', 'sample.txt')
    Invoke-Git @('commit', '-q', '-m', 'feat: a thing')
    Invoke-Git @('tag', 'v2026.1.2.0')

    $sha = (git rev-parse 'v2026.1.2.0^{}').Trim().Substring(0, 12)
    $changelog = New-Changelog "# Handoff v2026.1.2.0`n`n## What's Changed`n`n### Features`n- feat: a thing by @RealWhyKnot in abc1234`n"

    $body = (& $script -Tag 'v2026.1.2.0' -Repo 'RealWhyKnot/Handoff' -ChangelogPath $changelog -ExeSha256 'ABC123' -PrevTag 'v2026.1.1.0' -TemplateDir $templates) -join "`n"

    Assert-Contains $body '# Handoff v2026.1.2.0'
    Assert-Contains $body '- feat: a thing by @RealWhyKnot in abc1234'
    Assert-Contains $body 'SHA256 of `handoff.exe`: `abc123`'
    Assert-Contains $body "- **Source:** commit ``$sha`` on ``main``"
    Assert-Contains $body '## Install (fresh)'
    Assert-Contains $body '## Uninstall'
    Assert-Contains $body '## What you need to do'
    if ($body.IndexOf('# Handoff v2026.1.2.0') -ne 0) { throw 'The changelog must lead the body' }

    $noHash = (& $script -Tag 'v2026.1.2.0' -Repo 'RealWhyKnot/Handoff' -ChangelogPath $changelog -PrevTag '' -TemplateDir $templates) -join "`n"
    if ($noHash -match 'File integrity') { throw 'File integrity section must be omitted without a hash' }

    $missing = Join-Path $repo 'does-not-exist.md'
    $threw = $false
    try { & $script -Tag 'v2026.1.2.0' -Repo 'RealWhyKnot/Handoff' -ChangelogPath $missing -TemplateDir $templates | Out-Null } catch { $threw = $true }
    if (-not $threw) { throw 'A missing changelog must throw' }

    $empty = New-Changelog "   `n"
    $threw = $false
    try { & $script -Tag 'v2026.1.2.0' -Repo 'RealWhyKnot/Handoff' -ChangelogPath $empty -TemplateDir $templates | Out-Null } catch { $threw = $true }
    if (-not $threw) { throw 'An empty changelog must throw' }

    $unicode = New-Changelog ("## What's Changed`n`n- feat: caf" + [char]0x00E9 + " support by @RealWhyKnot in abc1234`n")
    $threw = $false
    try { & $script -Tag 'v2026.1.2.0' -Repo 'RealWhyKnot/Handoff' -ChangelogPath $unicode -TemplateDir $templates | Out-Null } catch { $threw = $true }
    if (-not $threw) { throw 'Non-ASCII in the changelog must throw' }

    Write-Host 'New-ReleaseNotes.ps1 tests passed.'
}
finally {
    Pop-Location
    if (Test-Path -LiteralPath $repo) { Remove-Item -LiteralPath $repo -Recurse -Force }
}

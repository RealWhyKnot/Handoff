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
    if ($LASTEXITCODE -ne 0) { throw "git $($Arguments -join ' ') failed with exit code $LASTEXITCODE" }
}

function Assert-True {
    [CmdletBinding()]
    [OutputType([void])]
    param(
        [Parameter(Mandatory)]
        [bool] $Condition,
        [Parameter(Mandatory)]
        [string] $Message
    )

    if (-not $Condition) {
        $script:failures++
        Write-Host "FAIL: $Message"
    }
}

function Assert-Contain {
    [CmdletBinding()]
    [OutputType([void])]
    param(
        [Parameter(Mandatory)]
        [string] $Body,
        [Parameter(Mandatory)]
        [string] $Expected
    )

    Assert-True -Condition $Body.Contains($Expected) -Message "Expected release body to contain: $Expected`n---`n$Body"
}

function New-Changelog {
    [CmdletBinding()]
    [OutputType([string])]
    param(
        [Parameter(Mandatory)]
        [string] $RepoRoot,
        [Parameter(Mandatory)]
        [string] $Text
    )

    $path = Join-Path $RepoRoot ('changelog-' + [guid]::NewGuid().ToString('N') + '.md')
    [IO.File]::WriteAllText($path, $Text)
    $path
}

function Test-NotesFailure {
    [CmdletBinding()]
    [OutputType([bool])]
    param(
        [Parameter(Mandatory)]
        [string] $Script,
        [Parameter(Mandatory)]
        [hashtable] $Common,
        [Parameter(Mandatory)]
        [string] $Changelog
    )

    try {
        $null = & $Script @Common -ChangelogPath $Changelog
    } catch [System.Management.Automation.RuntimeException] {
        return $true
    }
    $LASTEXITCODE -ne 0
}

$script:failures = 0
$notes = Join-Path $PSScriptRoot 'New-ReleaseNotes.ps1'
$common = @{
    Tag         = 'v2026.1.2.0'
    Repo        = 'RealWhyKnot/Handoff'
    TemplateDir = Join-Path $PSScriptRoot '..\release-template'
}
$repo = Join-Path ([IO.Path]::GetTempPath()) ('release-notes-test-' + [guid]::NewGuid().ToString('N'))
$null = [IO.Directory]::CreateDirectory($repo)
Push-Location -LiteralPath $repo
try {
    Invoke-Git -Arguments @('init', '-q')
    Invoke-Git -Arguments @('config', 'user.email', 'release-test@example.com')
    Invoke-Git -Arguments @('config', 'user.name', 'WhyKnot')
    Invoke-Git -Arguments @('config', 'commit.gpgsign', 'false')
    [IO.File]::WriteAllText((Join-Path $repo 'sample.txt'), "sample`n")
    Invoke-Git -Arguments @('add', 'sample.txt')
    Invoke-Git -Arguments @('commit', '-q', '-m', 'feat: a thing')
    Invoke-Git -Arguments @('tag', 'v2026.1.2.0')

    $sha = git rev-parse 'v2026.1.2.0^{}'
    if ($LASTEXITCODE -ne 0) { throw "git rev-parse failed with exit code $LASTEXITCODE" }
    $sha = ([string]$sha).Trim().Substring(0, 12)
    $entry = '- feat: a thing by @RealWhyKnot in abc1234'
    $text = "# Handoff v2026.1.2.0`n`n## What's Changed`n`n### Features`n$entry`n"
    $changelog = New-Changelog -RepoRoot $repo -Text $text

    $body = (& $notes @common -ChangelogPath $changelog -ExeSha256 'ABC123' -PrevTag 'v2026.1.1.0') -join "`n"
    if ($LASTEXITCODE -ne 0) { throw "New-ReleaseNotes.ps1 failed with exit code $LASTEXITCODE" }

    Assert-Contain -Body $body -Expected '# Handoff v2026.1.2.0'
    Assert-Contain -Body $body -Expected $entry
    Assert-Contain -Body $body -Expected 'SHA256 of `handoff.exe`: `abc123`'
    Assert-Contain -Body $body -Expected "Built from commit ``$sha`` on ``main``"
    Assert-Contain -Body $body -Expected '## Install (fresh)'
    Assert-Contain -Body $body -Expected '## Uninstall'
    Assert-Contain -Body $body -Expected '## What you need to do'
    Assert-True -Condition ($body.IndexOf('# Handoff v2026.1.2.0') -eq 0) -Message 'The changelog must lead the body'

    $noHash = (& $notes @common -ChangelogPath $changelog -PrevTag '') -join "`n"
    if ($LASTEXITCODE -ne 0) { throw "New-ReleaseNotes.ps1 failed with exit code $LASTEXITCODE" }
    $message = 'File integrity section must be omitted without a hash'
    Assert-True -Condition ($noHash -notmatch 'File integrity') -Message $message

    $missing = Join-Path $repo 'does-not-exist.md'
    $threw = Test-NotesFailure -Script $notes -Common $common -Changelog $missing
    Assert-True -Condition $threw -Message 'A missing changelog must throw'

    $empty = New-Changelog -RepoRoot $repo -Text "   `n"
    $threw = Test-NotesFailure -Script $notes -Common $common -Changelog $empty
    Assert-True -Condition $threw -Message 'An empty changelog must throw'

    $accented = "## What's Changed`n`n- feat: caf" + [char]0x00E9 + " support by @RealWhyKnot in abc1234`n"
    $unicode = New-Changelog -RepoRoot $repo -Text $accented
    $threw = Test-NotesFailure -Script $notes -Common $common -Changelog $unicode
    Assert-True -Condition $threw -Message 'Non-ASCII in the changelog must throw'
} finally {
    Pop-Location
    if (Test-Path -LiteralPath $repo) { Remove-Item -LiteralPath $repo -Recurse -Force }
}

if ($script:failures -gt 0) {
    Write-Host "$script:failures New-ReleaseNotes.ps1 checks failed."
    exit 1
}
Write-Host 'New-ReleaseNotes.ps1 tests passed.'
exit 0

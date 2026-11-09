#!/usr/bin/env pwsh
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-TestGit {
    [CmdletBinding()]
    [OutputType([void])]
    param(
        [Parameter(Mandatory)]
        [string] $RepoRoot,
        [Parameter(Mandatory)]
        [string[]] $Arguments
    )

    Push-Location -LiteralPath $RepoRoot
    try {
        $null = git @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "git $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}

function Write-TestFile {
    [CmdletBinding()]
    [OutputType([void])]
    param(
        [Parameter(Mandatory)]
        [string] $Path,
        [Parameter(Mandatory)]
        [string] $Content
    )

    $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $Path))
    [IO.File]::WriteAllText($Path, $Content)
}

function New-TestRepo {
    [CmdletBinding()]
    [OutputType([string])]
    param()

    $name = 'handoff-nightly-beta-' + [guid]::NewGuid().ToString('N')
    $root = Join-Path ([IO.Path]::GetTempPath()) $name
    $null = [IO.Directory]::CreateDirectory($root)

    Invoke-TestGit -RepoRoot $root -Arguments @('init', '-q', '.')
    Invoke-TestGit -RepoRoot $root -Arguments @('config', 'user.name', 'Handoff Tests')
    Invoke-TestGit -RepoRoot $root -Arguments @('config', 'user.email', 'handoff-tests@example.invalid')
    Invoke-TestGit -RepoRoot $root -Arguments @('config', 'core.autocrlf', 'false')

    Write-TestFile -Path (Join-Path $root 'sample.txt') -Content "base`n"
    Invoke-TestGit -RepoRoot $root -Arguments @('add', '.')
    Invoke-TestGit -RepoRoot $root -Arguments @('commit', '-q', '-m', 'initial')
    $root
}

function Invoke-Plan {
    [CmdletBinding()]
    [OutputType([System.Management.Automation.PSCustomObject])]
    param(
        [Parameter(Mandatory)]
        [string] $Planner,
        [Parameter(Mandatory)]
        [string] $RepoRoot,
        [string] $BaseTag = '',
        [string] $Tag = '',
        [string] $Today = '',
        [datetime] $NowUtc = ([datetime]::Parse('2026-06-16T01:30:00Z'))
    )

    $outputPath = Join-Path $RepoRoot 'plan.json'
    $arguments = @{
        RepoRoot       = $RepoRoot
        OutputJsonPath = $outputPath
        NowUtc         = $NowUtc
    }
    if (-not [string]::IsNullOrWhiteSpace($BaseTag)) {
        $arguments['BaseTag'] = $BaseTag
    }
    if (-not [string]::IsNullOrWhiteSpace($Tag)) {
        $arguments['Tag'] = $Tag
    }
    if (-not [string]::IsNullOrWhiteSpace($Today)) {
        $arguments['Today'] = $Today
    }

    & $Planner @arguments | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "Planner failed with exit code $LASTEXITCODE"
    }
    [IO.File]::ReadAllText($outputPath) | ConvertFrom-Json
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
        Write-Host "FAIL: $Message. Expected '$Expected', got '$Actual'."
    }
}

$script:failures = 0
$planner = Join-Path $PSScriptRoot 'Get-NightlyBetaPlan.ps1'
$repo = New-TestRepo
try {
    $plan = Invoke-Plan -Planner $planner -RepoRoot $repo
    $want = @{ Actual = $plan.has_changes; Expected = $true }
    Assert-Equal @want -Message 'Repos without release tags should publish'
    $want = @{ Actual = $plan.next_tag; Expected = 'v2026.6.15.0-beta' }
    Assert-Equal @want -Message 'Default beta tag date should use Central release date'

    Invoke-TestGit -RepoRoot $repo -Arguments @('tag', 'v2026.6.1.0')
    $plan = Invoke-Plan -Planner $planner -RepoRoot $repo
    $want = @{ Actual = $plan.has_changes; Expected = $false }
    Assert-Equal @want -Message 'Latest tag at HEAD should skip nightly beta tag creation'
    $want = @{ Actual = $plan.commit_count; Expected = 0 }
    Assert-Equal @want -Message 'Commit count should be zero at the latest tag'

    Write-TestFile -Path (Join-Path $repo 'sample.txt') -Content "changed`n"
    Invoke-TestGit -RepoRoot $repo -Arguments @('add', '.')
    Invoke-TestGit -RepoRoot $repo -Arguments @('commit', '-q', '-m', 'feat: add nightly input')
    $plan = Invoke-Plan -Planner $planner -RepoRoot $repo -Today '2026.6.9'
    $want = @{ Actual = $plan.has_changes; Expected = $true }
    Assert-Equal @want -Message 'Commits since the latest tag should create a plan'
    $want = @{ Actual = $plan.commit_count; Expected = 1 }
    Assert-Equal @want -Message 'Planner should count non-merge commits since the latest tag'
    $want = @{ Actual = $plan.next_tag; Expected = 'v2026.6.9.0-beta' }
    Assert-Equal @want -Message 'First same-day beta tag should use sequence zero'

    Invoke-TestGit -RepoRoot $repo -Arguments @('tag', 'v2026.6.9.0-beta')
    Invoke-TestGit -RepoRoot $repo -Arguments @('tag', 'v2026.6.9.1')
    $plan = Invoke-Plan -Planner $planner -RepoRoot $repo -Today '2026.6.9'
    $want = @{ Actual = $plan.next_tag; Expected = 'v2026.6.9.2-beta' }
    Assert-Equal @want -Message 'Same-day tags should increment from the highest release revision'

    $failed = $false
    try {
        $null = Invoke-Plan -Planner $planner -RepoRoot $repo -Tag 'v2026.6.9.2-beta.1'
    } catch [System.Management.Automation.RuntimeException] {
        $failed = $true
    }
    Assert-Equal -Actual $failed -Expected $true -Message 'Planner should reject numbered beta suffixes'
} finally {
    if (Test-Path -LiteralPath $repo) {
        Remove-Item -LiteralPath $repo -Recurse -Force
    }
}

if ($script:failures -gt 0) {
    Write-Host "$script:failures nightly beta planner checks failed."
    exit 1
}
Write-Host 'Nightly beta planner tests passed.'
exit 0

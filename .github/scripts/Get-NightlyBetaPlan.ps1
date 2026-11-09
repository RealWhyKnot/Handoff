#!/usr/bin/env pwsh
[CmdletBinding()]
param(
    [string] $RepoRoot = '',
    [string] $BaseTag = '',
    [string] $Tag = '',
    [string] $Today = '',
    [datetime] $NowUtc = ([datetime]::UtcNow),
    [string] $OutputJsonPath = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Resolve-RepoRoot {
    [CmdletBinding()]
    [OutputType([string])]
    param([string] $Value)

    if (-not [string]::IsNullOrWhiteSpace($Value)) {
        return (Resolve-Path -LiteralPath $Value).Path
    }
    (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
}

function Invoke-Git {
    [CmdletBinding()]
    [OutputType([object[]])]
    param(
        [Parameter(Mandatory)][string[]] $Arguments,
        [switch] $AllowFailure
    )

    $output = & { $ErrorActionPreference = 'Continue'; git @Arguments 2>$null }
    if ($LASTEXITCODE -ne 0) {
        if ($AllowFailure) {
            return @()
        }
        throw "git $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
    $output
}

function Get-FirstOutput {
    [CmdletBinding()]
    [OutputType([string])]
    param([object] $Value)

    $items = @($Value)
    if ($items.Count -eq 0) {
        return ''
    }
    [string]$items[0]
}

function Test-GitTag {
    [CmdletBinding()]
    [OutputType([bool])]
    param([string] $Name)

    if ([string]::IsNullOrWhiteSpace($Name)) {
        return $false
    }
    $null = & { $ErrorActionPreference = 'Continue'; git rev-parse --verify --quiet "refs/tags/$Name^{commit}" 2>$null }
    if ($LASTEXITCODE -ne 0) {
        return $false
    }
    $true
}

function Get-CentralTimeZone {
    [CmdletBinding()]
    [OutputType([System.TimeZoneInfo])]
    param()

    foreach ($id in @('Central Standard Time', 'America/Chicago')) {
        try {
            return [System.TimeZoneInfo]::FindSystemTimeZoneById($id)
        } catch [System.TimeZoneNotFoundException] {
            continue
        }
    }
    throw 'Could not resolve the America/Chicago release time zone.'
}

function Get-ReleaseDateStamp {
    [CmdletBinding()]
    [OutputType([string])]
    param(
        [Parameter(Mandatory)][datetime] $CurrentUtc,
        [Parameter(Mandatory)][string] $Format
    )

    $utc = $CurrentUtc
    if ($utc.Kind -ne [System.DateTimeKind]::Utc) {
        $utc = $utc.ToUniversalTime()
    }
    $central = [System.TimeZoneInfo]::ConvertTimeFromUtc($utc, (Get-CentralTimeZone))
    $central.ToString($Format, [System.Globalization.CultureInfo]::InvariantCulture)
}

function Get-LatestReleaseTag {
    [CmdletBinding()]
    [OutputType([string])]
    param([string] $Override)

    if (-not [string]::IsNullOrWhiteSpace($Override)) {
        return $Override
    }

    $describe = @('describe', '--tags', '--abbrev=0', '--match', 'v*', 'HEAD')
    $latestTag = Get-FirstOutput -Value (Invoke-Git -Arguments $describe -AllowFailure)
    $latestTag.Trim()
}

function Get-CommitCountSinceTag {
    [CmdletBinding()]
    [OutputType([int])]
    param([string] $Name)

    $range = 'HEAD'
    if (-not [string]::IsNullOrWhiteSpace($Name) -and (Test-GitTag -Name $Name)) {
        $range = "$Name..HEAD"
    }

    $count = Get-FirstOutput -Value (Invoke-Git -Arguments @('rev-list', '--count', '--no-merges', $range))
    [int]$count.Trim()
}

function Get-NextBetaTag {
    [CmdletBinding()]
    [OutputType([string])]
    param(
        [string] $DateStamp,
        [Parameter(Mandatory)][datetime] $CurrentUtc
    )

    if ([string]::IsNullOrWhiteSpace($DateStamp)) {
        $DateStamp = Get-ReleaseDateStamp -CurrentUtc $CurrentUtc -Format 'yyyy.M.d'
    }

    $escaped = [regex]::Escape($DateStamp)
    $pattern = "^v$escaped\.(\d+)(-[A-Za-z0-9]{4})?$"
    $highest = -1
    foreach ($existing in @(Invoke-Git -Arguments @('tag', '--list', "v$DateStamp.*"))) {
        if ($existing -match $pattern) {
            $value = [int]$Matches[1]
            if ($value -gt $highest) {
                $highest = $value
            }
        }
    }

    "v$DateStamp.$($highest + 1)-beta"
}

function Write-GitHubOutput {
    [CmdletBinding()]
    [OutputType([void])]
    param(
        [Parameter(Mandatory)][string] $Name,
        [string] $Value
    )

    if ([string]::IsNullOrWhiteSpace($env:GITHUB_OUTPUT)) {
        return
    }
    [IO.File]::AppendAllText($env:GITHUB_OUTPUT, "$Name=$Value`n")
}

$repoRootPath = Resolve-RepoRoot -Value $RepoRoot
Push-Location -LiteralPath $repoRootPath
try {
    if (-not [string]::IsNullOrWhiteSpace($Tag) -and $Tag -notmatch '^v\d{4}\.\d+\.\d+\.\d+-beta$') {
        throw 'Nightly beta tags must match vYYYY.M.D.N-beta.'
    }

    $head = (Get-FirstOutput -Value (Invoke-Git -Arguments @('rev-parse', 'HEAD'))).Trim()
    $base = Get-LatestReleaseTag -Override $BaseTag
    $commitCount = Get-CommitCountSinceTag -Name $base
    $hasChanges = $commitCount -gt 0
    if ([string]::IsNullOrWhiteSpace($Tag)) {
        $nextTag = Get-NextBetaTag -DateStamp $Today -CurrentUtc $NowUtc
    } else {
        $nextTag = $Tag
    }

    $plan = [pscustomobject]@{
        has_changes  = $hasChanges
        commit_count = $commitCount
        base_tag     = $base
        head_sha     = $head
        next_tag     = $nextTag
    }

    if (-not [string]::IsNullOrWhiteSpace($OutputJsonPath)) {
        $resolvedOutputPath = [System.IO.Path]::GetFullPath($OutputJsonPath)
        $null = [IO.Directory]::CreateDirectory((Split-Path -Parent $resolvedOutputPath))
        $json = $plan | ConvertTo-Json -Depth 4 -Compress
        [IO.File]::WriteAllText($resolvedOutputPath, $json)
    }

    Write-GitHubOutput -Name 'has_changes' -Value ([string]$hasChanges).ToLowerInvariant()
    Write-GitHubOutput -Name 'commit_count' -Value ([string]$commitCount)
    Write-GitHubOutput -Name 'base_tag' -Value $base
    Write-GitHubOutput -Name 'head_sha' -Value $head
    Write-GitHubOutput -Name 'next_tag' -Value $nextTag

    Write-Host 'Nightly beta release plan:'
    Write-Host "  Base tag: $base"
    Write-Host "  Head: $head"
    Write-Host "  Commit count: $commitCount"
    Write-Host "  Next tag: $nextTag"
    Write-Host "  Has changes: $hasChanges"
} finally {
    Pop-Location
}
exit 0

#!/usr/bin/env pwsh
[CmdletBinding()]
param(
    [string] $RepoRoot = "",
    [string] $BaseTag = "",
    [string] $Tag = "",
    [string] $Today = "",
    [datetime] $NowUtc = ([datetime]::UtcNow),
    [string] $OutputJsonPath = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Resolve-RepoRoot {
    param([string] $Value)

    if (-not [string]::IsNullOrWhiteSpace($Value)) {
        return (Resolve-Path -LiteralPath $Value).Path
    }
    return (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..\..")).Path
}

function Invoke-Git {
    param(
        [string[]] $Arguments,
        [switch] $AllowFailure
    )

    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & git @Arguments 2>$null
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousPreference
    }

    if ($exitCode -ne 0) {
        if ($AllowFailure) {
            return $null
        }
        throw "git $($Arguments -join ' ') failed with exit code $exitCode"
    }
    return @($output)
}

function Get-FirstOutput {
    param($Value)

    $items = @($Value)
    if ($items.Count -eq 0) {
        return $null
    }
    return [string]$items[0]
}

function Test-GitTagExists {
    param([string] $Name)

    if ([string]::IsNullOrWhiteSpace($Name)) {
        return $false
    }
    & git rev-parse --verify --quiet "refs/tags/$Name^{commit}" *> $null
    return ($LASTEXITCODE -eq 0)
}

function Get-CentralTimeZone {
    foreach ($id in @("Central Standard Time", "America/Chicago")) {
        try {
            return [System.TimeZoneInfo]::FindSystemTimeZoneById($id)
        }
        catch {
        }
    }
    throw "Could not resolve the America/Chicago release time zone."
}

function Get-ReleaseDateStamp {
    param(
        [datetime] $CurrentUtc,
        [string] $Format
    )

    $utc = $CurrentUtc
    if ($utc.Kind -ne [System.DateTimeKind]::Utc) {
        $utc = $utc.ToUniversalTime()
    }
    $central = [System.TimeZoneInfo]::ConvertTimeFromUtc($utc, (Get-CentralTimeZone))
    return $central.ToString($Format, [System.Globalization.CultureInfo]::InvariantCulture)
}

function Get-LatestReleaseTag {
    param([string] $Override)

    if (-not [string]::IsNullOrWhiteSpace($Override)) {
        return $Override
    }

    $latestTag = Get-FirstOutput -Value (Invoke-Git -Arguments @("describe", "--tags", "--abbrev=0", "--match", "v*", "HEAD") -AllowFailure)
    if ([string]::IsNullOrWhiteSpace($latestTag)) {
        return ""
    }
    return $latestTag.Trim()
}

function Get-CommitCountSinceTag {
    param([string] $Name)

    $range = "HEAD"
    if (-not [string]::IsNullOrWhiteSpace($Name) -and (Test-GitTagExists -Name $Name)) {
        $range = "$Name..HEAD"
    }

    $count = Get-FirstOutput -Value (Invoke-Git -Arguments @("rev-list", "--count", "--no-merges", $range))
    return [int]$count.Trim()
}

function Get-NextBetaTag {
    param(
        [string] $DateStamp,
        [datetime] $CurrentUtc
    )

    if ([string]::IsNullOrWhiteSpace($DateStamp)) {
        $DateStamp = Get-ReleaseDateStamp -CurrentUtc $CurrentUtc -Format "yyyy.M.d"
    }

    $escaped = [regex]::Escape($DateStamp)
    $pattern = "^v$escaped\.(\d+)(-[A-Za-z0-9]{4})?$"
    $highest = -1
    foreach ($existing in @(Invoke-Git -Arguments @("tag", "--list", "v$DateStamp.*"))) {
        if ($existing -match $pattern) {
            $value = [int]$Matches[1]
            if ($value -gt $highest) {
                $highest = $value
            }
        }
    }

    return "v$DateStamp.$($highest + 1)-beta"
}

function Write-GitHubOutput {
    param(
        [string] $Name,
        [string] $Value
    )

    if ([string]::IsNullOrWhiteSpace($env:GITHUB_OUTPUT)) {
        return
    }
    $encoding = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::AppendAllText($env:GITHUB_OUTPUT, "$Name=$Value`n", $encoding)
}

$repoRootPath = Resolve-RepoRoot -Value $RepoRoot
Push-Location $repoRootPath
try {
    if (-not [string]::IsNullOrWhiteSpace($Tag) -and $Tag -notmatch "^v\d{4}\.\d+\.\d+\.\d+-beta$") {
        throw "Nightly beta tags must match vYYYY.M.D.N-beta."
    }

    $head = (Get-FirstOutput -Value (Invoke-Git -Arguments @("rev-parse", "HEAD"))).Trim()
    $base = Get-LatestReleaseTag -Override $BaseTag
    $commitCount = Get-CommitCountSinceTag -Name $base
    $hasChanges = $commitCount -gt 0
    if ([string]::IsNullOrWhiteSpace($Tag)) {
        $nextTag = Get-NextBetaTag -DateStamp $Today -CurrentUtc $NowUtc
    }
    else {
        $nextTag = $Tag
    }

    $plan = [pscustomobject]@{
        has_changes = $hasChanges
        commit_count = $commitCount
        base_tag = $base
        head_sha = $head
        next_tag = $nextTag
    }

    if (-not [string]::IsNullOrWhiteSpace($OutputJsonPath)) {
        $resolvedOutputPath = [System.IO.Path]::GetFullPath($OutputJsonPath)
        $parent = Split-Path -Parent $resolvedOutputPath
        if (-not (Test-Path -LiteralPath $parent)) {
            New-Item -ItemType Directory -Path $parent | Out-Null
        }
        $json = $plan | ConvertTo-Json -Depth 4 -Compress
        $encoding = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText($resolvedOutputPath, $json, $encoding)
    }

    Write-GitHubOutput -Name "has_changes" -Value ([string]$hasChanges).ToLowerInvariant()
    Write-GitHubOutput -Name "commit_count" -Value ([string]$commitCount)
    Write-GitHubOutput -Name "base_tag" -Value $base
    Write-GitHubOutput -Name "head_sha" -Value $head
    Write-GitHubOutput -Name "next_tag" -Value $nextTag

    Write-Host "Nightly beta release plan:"
    Write-Host "  Base tag: $base"
    Write-Host "  Head: $head"
    Write-Host "  Commit count: $commitCount"
    Write-Host "  Next tag: $nextTag"
    Write-Host "  Has changes: $hasChanges"
}
finally {
    Pop-Location
}

#!/usr/bin/env pwsh
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Tag,
    [Parameter(Mandatory = $true)]
    [string] $Repo,
    [string] $ExeSha256 = '',
    [string] $PrevTag = '',
    [string] $TemplateDir = (Join-Path $PSScriptRoot '..\release-template')
)

$ErrorActionPreference = 'Stop'

if (-not $PSBoundParameters.ContainsKey('PrevTag')) {
    $PrevTag = & (Join-Path $PSScriptRoot 'Resolve-ReleaseBaseTag.ps1') -Tag $Tag
}

[string[]] $logArgs = if ($PrevTag) { @("$PrevTag..$Tag") } else { @($Tag) }
$raw = & git log @logArgs --no-merges --pretty=format:"%h`t%an`t%s"
if ($LASTEXITCODE -ne 0) { throw "git log $logArgs failed" }

$authorMap = @{ 'WhyKnot' = 'RealWhyKnot' }
$typeNames = [ordered]@{
    feat     = 'Features'
    fix      = 'Bug Fixes'
    perf     = 'Performance'
    refactor = 'Refactors'
    revert   = 'Reverts'
    docs     = 'Documentation'
    style    = 'Style'
    test     = 'Tests'
    ci       = 'CI'
    build    = 'Build'
    chore    = 'Chores'
    other    = 'Other Changes'
}

$groups = @{}
foreach ($line in @($raw)) {
    if ([string]::IsNullOrWhiteSpace($line) -or $line -match '\[skip changelog\]') { continue }
    $parts = $line -split "`t", 3
    if ($parts.Count -lt 3) { continue }
    $short = $parts[0]
    $author = $parts[1]
    if ($authorMap.ContainsKey($author)) { $author = $authorMap[$author] }
    $subject = $parts[2] -replace '\s*\(\d{4}\.\d+\.\d+\.\d+-[A-Fa-f0-9]+\)\s*', ' '
    $subject = ($subject.Trim() -replace '\s{2,}', ' ')
    $type = 'other'
    if ($subject -match '^(?<type>[a-z]+)(\(.+?\))?!?:') {
        $t = $matches['type']
        if ($typeNames.Contains($t)) { $type = $t }
    }
    if (-not $groups.ContainsKey($type)) { $groups[$type] = New-Object System.Collections.Generic.List[string] }
    $groups[$type].Add("- $subject by @$author in $short")
}
if ($groups.Count -eq 0) { throw "No commits found for $Tag (range: $($logArgs -join ' '))" }

$tagSha = (& git rev-parse "$Tag^{}").Trim()
if ($LASTEXITCODE -ne 0) { throw "git rev-parse $Tag failed" }
$owner, $repoShort = $Repo -split '/', 2
$tokens = @{
    '{tag}'              = $Tag
    '{version}'          = ($Tag -replace '^v', '')
    '{owner}'            = $owner
    '{repo}'             = $repoShort
    '{full-repo}'        = $Repo
    '{commit-sha}'       = $tagSha
    '{commit-sha-short}' = $tagSha.Substring(0, 12)
    '{prior-tag}'        = [string]$PrevTag
}

$sb = [System.Text.StringBuilder]::new()
[void]$sb.AppendLine("# $repoShort $Tag")
[void]$sb.AppendLine()
[void]$sb.AppendLine("## What's Changed")
[void]$sb.AppendLine()
foreach ($key in $typeNames.Keys) {
    if (-not $groups.ContainsKey($key)) { continue }
    [void]$sb.AppendLine("### $($typeNames[$key])")
    foreach ($e in $groups[$key]) { [void]$sb.AppendLine($e) }
    [void]$sb.AppendLine()
}
if ($PrevTag) {
    [void]$sb.AppendLine("**Full Changelog**: https://github.com/$Repo/compare/$PrevTag...$Tag")
    [void]$sb.AppendLine()
}
if ($ExeSha256) {
    [void]$sb.AppendLine("## File integrity")
    [void]$sb.AppendLine()
    [void]$sb.AppendLine("SHA256 of ``handoff.exe``: ``$($ExeSha256.ToLower())``. The same hash is in ``handoff-version.json`` next to it, which ``handoff update`` reads.")
    [void]$sb.AppendLine()
}
foreach ($name in @('links', 'install', 'uninstall', 'what-you-need-to-do')) {
    $path = Join-Path $TemplateDir "$name.md"
    if (-not (Test-Path -LiteralPath $path)) { throw "Release template missing: $path" }
    $section = (Get-Content -LiteralPath $path -Raw -Encoding UTF8).Trim()
    foreach ($k in $tokens.Keys) { $section = $section.Replace($k, [string]$tokens[$k]) }
    [void]$sb.AppendLine($section)
    [void]$sb.AppendLine()
}

$body = ($sb.ToString() -replace "`r`n", "`n").TrimEnd()
$bad = [regex]::Matches($body, '[^\x09\x0A\x0D\x20-\x7E]')
if ($bad.Count -gt 0) {
    $codes = ($bad | ForEach-Object { 'U+{0:X4}' -f [int][char]$_.Value } | Select-Object -Unique) -join ' '
    throw "Non-ASCII characters in release body: $codes. Amend the offending commit subject or template."
}
$body

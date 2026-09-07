#!/usr/bin/env pwsh
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Tag,
    [Parameter(Mandatory = $true)]
    [string] $Repo,
    [Parameter(Mandatory = $true)]
    [string] $ChangelogPath,
    [string] $ExeSha256 = '',
    [string] $PrevTag = '',
    [string] $TemplateDir = (Join-Path $PSScriptRoot '..\release-template')
)

$ErrorActionPreference = 'Stop'

if (-not $PSBoundParameters.ContainsKey('PrevTag')) {
    $PrevTag = & (Join-Path $PSScriptRoot 'Resolve-ReleaseBaseTag.ps1') -Tag $Tag
}

if (-not (Test-Path -LiteralPath $ChangelogPath)) {
    throw "Changelog not found at $ChangelogPath. It comes from RealWhyKnot/workflows/release-notes."
}
$changelog = (Get-Content -LiteralPath $ChangelogPath -Raw -Encoding UTF8).Trim()
if (-not $changelog) { throw "Changelog at $ChangelogPath is empty." }

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
[void]$sb.AppendLine($changelog)
[void]$sb.AppendLine()
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

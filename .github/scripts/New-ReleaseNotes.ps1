#!/usr/bin/env pwsh
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $Tag,
    [Parameter(Mandatory)]
    [string] $Repo,
    [Parameter(Mandatory)]
    [string] $ChangelogPath,
    [string] $ExeSha256 = '',
    [string] $PrevTag = '',
    [string] $TemplateDir = (Join-Path $PSScriptRoot '..\release-template')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $PSBoundParameters.ContainsKey('PrevTag')) {
    $PrevTag = & (Join-Path $PSScriptRoot 'Resolve-ReleaseBaseTag.ps1') -Tag $Tag
    if ($LASTEXITCODE -ne 0) { throw "Resolve-ReleaseBaseTag.ps1 failed with exit code $LASTEXITCODE" }
}

if (-not (Test-Path -LiteralPath $ChangelogPath)) {
    throw "Changelog not found at $ChangelogPath. It comes from RealWhyKnot/workflows/release-notes."
}
$changelog = [IO.File]::ReadAllText((Resolve-Path -LiteralPath $ChangelogPath).Path).Trim()
if (-not $changelog) { throw "Changelog at $ChangelogPath is empty." }

$tagSha = git rev-parse "$Tag^{}"
if ($LASTEXITCODE -ne 0) { throw "git rev-parse $Tag failed with exit code $LASTEXITCODE" }
$tagSha = ([string]$tagSha).Trim()
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
$null = $sb.AppendLine($changelog)
$null = $sb.AppendLine()
if ($ExeSha256) {
    $hash = $ExeSha256.ToLower()
    $integrity = @(
        "SHA256 of ``handoff.exe``: ``$hash``. The same hash is in ``handoff-version.json`` next to it,"
        'which `handoff update` reads.'
    ) -join ' '
    $null = $sb.AppendLine('## File integrity')
    $null = $sb.AppendLine()
    $null = $sb.AppendLine($integrity)
    $null = $sb.AppendLine()
}
foreach ($name in @('links', 'install', 'uninstall', 'what-you-need-to-do')) {
    $path = Join-Path $TemplateDir "$name.md"
    if (-not (Test-Path -LiteralPath $path)) { throw "Release template missing: $path" }
    $section = [IO.File]::ReadAllText((Resolve-Path -LiteralPath $path).Path).Trim()
    foreach ($k in $tokens.Keys) { $section = $section.Replace($k, [string]$tokens[$k]) }
    $null = $sb.AppendLine($section)
    $null = $sb.AppendLine()
}

$body = ($sb.ToString() -replace "`r`n", "`n").TrimEnd()
$bad = [regex]::Matches($body, '[^\x09\x0A\x0D\x20-\x7E]')
if ($bad.Count -gt 0) {
    $codes = @(foreach ($match in $bad) { 'U+{0:X4}' -f [int][char]$match.Value }) | Select-Object -Unique
    throw "Non-ASCII characters in release body: $($codes -join ' '). Amend the offending commit subject or template."
}
$body
exit 0

#!/usr/bin/env pwsh
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $Tag
)

$ErrorActionPreference = 'Stop'

function Test-IsPrereleaseTag([string]$Tag) {
    return $Tag -like '*-*'
}

$describeArgs = @('describe', '--tags', '--abbrev=0')
if (-not (Test-IsPrereleaseTag -Tag $Tag)) {
    $describeArgs += @('--exclude', '*-*')
}
$describeArgs += "$Tag^"

$prevRef = & git @describeArgs 2>$null
if ($LASTEXITCODE -eq 0 -and $prevRef) {
    $prevRef.Trim()
}

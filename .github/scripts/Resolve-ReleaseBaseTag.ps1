#!/usr/bin/env pwsh
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $Tag
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$exclude = @()
if ($Tag -notlike '*-*') {
    $exclude = @('--exclude', '*-*')
}
$describeArgs = @('describe', '--tags', '--abbrev=0') + $exclude + @("$Tag^")

$prevRef = & { $ErrorActionPreference = 'Continue'; git @describeArgs 2>$null }
if ($LASTEXITCODE -eq 0 -and $prevRef) {
    ([string]$prevRef).Trim()
}
exit 0

#!/usr/bin/env pwsh
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Step {
    [CmdletBinding()]
    [OutputType([void])]
    param(
        [Parameter(Mandatory)][string] $Name,
        [Parameter(Mandatory)][string] $Exe,
        [string[]] $Arguments = @()
    )
    Write-Host "== $Name"
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Name failed with exit code $LASTEXITCODE" }
}

function Resolve-Awk {
    [CmdletBinding()]
    [OutputType([string])]
    param()
    $onPath = Get-Command -Name awk -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($onPath) { return $onPath.Source }
    $git = Get-Command -Name git -CommandType Application | Select-Object -First 1
    $bundled = Join-Path (Split-Path -Parent (Split-Path -Parent $git.Source)) 'usr\bin\awk.exe'
    if (-not (Test-Path -LiteralPath $bundled)) { throw 'awk is required; it ships with Git for Windows' }
    $bundled
}

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
Set-Location -LiteralPath $repoRoot

git config --local core.hooksPath .githooks
if ($LASTEXITCODE -ne 0) { throw "git config failed with exit code $LASTEXITCODE" }

$rustFiles = @(git ls-files -- '*.rs')
if ($LASTEXITCODE -ne 0) { throw "git ls-files failed with exit code $LASTEXITCODE" }

Invoke-Step -Name 'style check' -Exe 'python' -Arguments @('-B', '.github/lint/style_check.py')
Invoke-Step -Name 'cargo fmt' -Exe 'cargo' -Arguments @('fmt', '--check')
$clippy = @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings')
Invoke-Step -Name 'cargo clippy' -Exe 'cargo' -Arguments $clippy
Invoke-Step -Name 'cargo test' -Exe 'cargo' -Arguments @('test', '--workspace', '--all-targets', '--locked')
if ($rustFiles.Count -gt 0) {
    Invoke-Step -Name 'rust style' -Exe (Resolve-Awk) -Arguments (@('-f', '.github/lint/rust-style.awk') + $rustFiles)
}
$psLint = @('-NoProfile', '-File', (Join-Path $PSScriptRoot 'Invoke-PSLint.ps1'))
Invoke-Step -Name 'PowerShell lint' -Exe 'pwsh' -Arguments $psLint

if ($env:OS -ne 'Windows_NT') {
    Invoke-Step -Name 'shell lint' -Exe 'bash' -Arguments @('.githooks/check-shell')
} elseif ($env:LINT_WSL_DISTRO) {
    $wsl = @('--distribution', $env:LINT_WSL_DISTRO, '--cd', $repoRoot, '--', 'bash', '.githooks/check-shell')
    Invoke-Step -Name 'shell lint' -Exe 'wsl' -Arguments $wsl
} else {
    Write-Host '== shell lint skipped: set LINT_WSL_DISTRO to a WSL distro with shellcheck and shfmt'
}
exit 0

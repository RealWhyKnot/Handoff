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

function Resolve-GitRoot {
    [CmdletBinding()]
    [OutputType([string])]
    param()
    $git = Get-Command -Name git -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $git) { return '' }
    $directory = Split-Path -Parent $git.Source
    while ($directory) {
        if (Test-Path -LiteralPath (Join-Path $directory 'bin\bash.exe')) { return $directory }
        $directory = Split-Path -Parent $directory
    }
    ''
}

function Test-ShellTool {
    [CmdletBinding()]
    [OutputType([bool])]
    param()
    foreach ($tool in 'shellcheck', 'shfmt') {
        if (-not (Get-Command -Name $tool -CommandType Application -ErrorAction SilentlyContinue)) { return $false }
    }
    $true
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

$gitRoot = if ($env:OS -eq 'Windows_NT') { Resolve-GitRoot } else { '' }
if ($env:OS -ne 'Windows_NT') {
    Invoke-Step -Name 'shell lint' -Exe 'bash' -Arguments @('.githooks/check-shell')
} elseif ($gitRoot -and (Test-ShellTool)) {
    $savedPath = $env:PATH
    $env:PATH = "$(Join-Path $gitRoot 'usr\bin');$env:PATH"
    try {
        Invoke-Step -Name 'shell lint' -Exe (Join-Path $gitRoot 'bin\bash.exe') -Arguments @('.githooks/check-shell')
    } finally {
        $env:PATH = $savedPath
    }
} elseif ($env:LINT_WSL_DISTRO) {
    $wsl = @('--distribution', $env:LINT_WSL_DISTRO, '--cd', $repoRoot, '--', 'bash', '.githooks/check-shell')
    Invoke-Step -Name 'shell lint' -Exe 'wsl' -Arguments $wsl
} else {
    Write-Host '== shell lint skipped: install shellcheck and shfmt, or set LINT_WSL_DISTRO'
}
exit 0

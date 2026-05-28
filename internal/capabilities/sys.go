// SPDX-License-Identifier: GPL-3.0-or-later
//
// Package capabilities holds the per-kind handlers wired into the
// dispatch router. Each file groups related kinds.

package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterSys attaches sys.* handlers to the given router.
func RegisterSys(r *dispatch.Router) {
	r.Register("sys.info", sysInfo)
	r.Register("sys.uptime", sysUptime)
	r.Register("sys.hotfixes", sysHotfixes)
	r.Register("sys.reboot-required", sysRebootRequired)
	r.Register("sys.env", sysEnv)
	r.Register("sys.users", sysUsers)
	r.Register("sys.timezone", sysTimezone)
	r.Register("sys.resources", sysResources)
}

func sysInfo(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	// Get-ComputerInfo returns a big object; ConvertTo-Json depth 4 is
	// enough to keep nested arrays from collapsing to "System.Object[]"
	// without ballooning the payload.
	script := `Get-ComputerInfo | ConvertTo-Json -Compress -Depth 4`
	return runPwshJSON(ctx, script)
}

func sysUptime(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
$os = Get-CimInstance Win32_OperatingSystem
$boot = $os.LastBootUpTime
$now = Get-Date
$up = ($now - $boot).TotalSeconds
[ordered]@{
    boot_utc = $boot.ToUniversalTime().ToString("o")
    uptime_seconds = [int64]$up
    last_boot_local = $boot.ToString("o")
} | ConvertTo-Json -Compress
`
	return runPwshJSON(ctx, script)
}

func sysHotfixes(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
Get-HotFix |
    Sort-Object InstalledOn -Descending |
    Select-Object HotFixID,Description,InstalledBy,@{n='InstalledOn';e={ if ($_.InstalledOn) { $_.InstalledOn.ToString("o") } else { $null } }} |
    ConvertTo-Json -Compress -Depth 4
`
	return runPwshJSON(ctx, script)
}

func sysEnv(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	scope := "all"
	if v, ok := args["scope"]; ok {
		_ = json.Unmarshal(v, &scope)
	}
	scope = strings.ToLower(strings.TrimSpace(scope))
	switch scope {
	case "", "all", "machine", "user", "process":
	default:
		return nil, fmt.Errorf("sys.env: scope must be all, machine, user, or process")
	}
	if scope == "" {
		scope = "all"
	}

	var prefix string
	if v, ok := args["name_prefix"]; ok {
		_ = json.Unmarshal(v, &prefix)
	}
	prefix = strings.TrimSpace(prefix)
	if len(prefix) > 128 {
		return nil, fmt.Errorf("sys.env: name_prefix too long")
	}

	script := fmt.Sprintf(`
$scope = %s
$prefix = %s
$entries = New-Object System.Collections.Generic.List[object]
function Add-Scope($name, $target) {
    foreach ($entry in [Environment]::GetEnvironmentVariables($target).GetEnumerator()) {
        $key = [string]$entry.Key
        if ($prefix -and -not $key.ToLowerInvariant().StartsWith($prefix.ToLowerInvariant())) { return }
        $entries.Add([ordered]@{
            scope = $name
            name  = $key
            value = [string]$entry.Value
        }) | Out-Null
    }
}
if ($scope -eq 'all' -or $scope -eq 'machine') { Add-Scope 'machine' 'Machine' }
if ($scope -eq 'all' -or $scope -eq 'user')    { Add-Scope 'user'    'User' }
if ($scope -eq 'all' -or $scope -eq 'process') { Add-Scope 'process' 'Process' }
[ordered]@{
    scope   = $scope
    prefix  = $prefix
    count   = $entries.Count
    entries = @($entries | Sort-Object scope, name)
} | ConvertTo-Json -Compress -Depth 4
`, psSingleQuote(scope), psSingleQuote(prefix))
	return runPwshJSON(ctx, script)
}

func sysUsers(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
$logon = Get-CimInstance Win32_LoggedOnUser -ErrorAction SilentlyContinue |
    ForEach-Object {
        $acc = $_.Antecedent
        if ($acc -match 'Domain="([^"]+)",Name="([^"]+)"') {
            [pscustomobject]@{
                domain = $matches[1]
                name   = $matches[2]
            }
        }
    } | Sort-Object -Property name -Unique

$sessions = @()
try {
    $raw = (& query.exe session) 2>$null
    if ($raw -and $LASTEXITCODE -eq 0) {
        $idx = 0
        foreach ($line in $raw) {
            $idx++
            if ($idx -eq 1) { continue }
            if (-not $line.Trim()) { continue }
            $sessions += [ordered]@{
                raw = $line.Trim()
            }
        }
    }
} catch {}

[ordered]@{
    accounts = @($logon | Select-Object -First 50)
    terminal_sessions = @($sessions | Select-Object -First 50)
} | ConvertTo-Json -Compress -Depth 4
`
	return runPwshJSON(ctx, script)
}

func sysTimezone(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
$tz = Get-TimeZone
$now = Get-Date
[ordered]@{
    id            = $tz.Id
    display_name  = $tz.DisplayName
    standard_name = $tz.StandardName
    base_utc_offset_minutes = [int]$tz.BaseUtcOffset.TotalMinutes
    supports_dst  = $tz.SupportsDaylightSavingTime
    is_dst_now    = $tz.IsDaylightSavingTime($now)
    local_now     = $now.ToString("o")
    utc_now       = $now.ToUniversalTime().ToString("o")
    culture       = (Get-Culture).Name
    ui_culture    = (Get-UICulture).Name
} | ConvertTo-Json -Compress
`
	return runPwshJSON(ctx, script)
}

func sysRebootRequired(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
$reasons = @()
if (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending') {
    $reasons += 'component_based_servicing'
}
if (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired') {
    $reasons += 'windows_update'
}
$sessionManager = Get-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager' -Name PendingFileRenameOperations -ErrorAction SilentlyContinue
if ($null -ne $sessionManager -and $sessionManager.PendingFileRenameOperations) {
    $reasons += 'pending_file_rename'
}
$computerName = $env:COMPUTERNAME
[ordered]@{
    required = ($reasons.Count -gt 0)
    reasons = @($reasons)
    computer_name = $computerName
} | ConvertTo-Json -Compress
`
	return runPwshJSON(ctx, script)
}

func resourceTopArg(args map[string]json.RawMessage) int {
	top := 10
	if v, ok := args["top"]; ok {
		_ = json.Unmarshal(v, &top)
	}
	if top <= 0 {
		top = 10
	}
	if top > 50 {
		top = 50
	}
	return top
}

func sysResources(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	top := resourceTopArg(args)
	script := fmt.Sprintf(`
$top = %d
$sampled = Get-Date
$os = Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue
$cpu = Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'" -ErrorAction SilentlyContinue
$page = Get-CimInstance Win32_PageFileUsage -ErrorAction SilentlyContinue |
    Select-Object @{n='name';e={[string]$_.Name}},
                  @{n='allocated_mb';e={[int]$_.AllocatedBaseSize}},
                  @{n='current_usage_mb';e={[int]$_.CurrentUsage}},
                  @{n='peak_usage_mb';e={[int]$_.PeakUsage}}

$procs = Get-Process -ErrorAction SilentlyContinue
$topByMemory = $procs |
    Sort-Object WorkingSet64 -Descending |
    Select-Object -First $top |
    Select-Object @{n='pid';e={[int]$_.Id}},
                  @{n='name';e={[string]$_.ProcessName}},
                  @{n='working_set_mb';e={[math]::Round($_.WorkingSet64 / 1MB, 1)}},
                  @{n='cpu_seconds';e={ if ($null -ne $_.CPU) { [math]::Round($_.CPU, 1) } else { $null } }}

$topByCpu = $procs |
    Where-Object { $null -ne $_.CPU } |
    Sort-Object CPU -Descending |
    Select-Object -First $top |
    Select-Object @{n='pid';e={[int]$_.Id}},
                  @{n='name';e={[string]$_.ProcessName}},
                  @{n='cpu_seconds';e={[math]::Round($_.CPU, 1)}},
                  @{n='working_set_mb';e={[math]::Round($_.WorkingSet64 / 1MB, 1)}}

$totalMB = if ($os) { [double]$os.TotalVisibleMemorySize / 1024 } else { $null }
$freeMB = if ($os) { [double]$os.FreePhysicalMemory / 1024 } else { $null }
$usedPct = $null
if ($null -ne $totalMB -and $totalMB -gt 0 -and $null -ne $freeMB) {
    $usedPct = [math]::Round((($totalMB - $freeMB) / $totalMB) * 100, 1)
}

[ordered]@{
    sampled_utc = $sampled.ToUniversalTime().ToString("o")
    top = $top
    cpu_percent = if ($cpu) { [int]$cpu.PercentProcessorTime } else { $null }
    memory_total_mb = if ($null -ne $totalMB) { [math]::Round($totalMB, 1) } else { $null }
    memory_free_mb = if ($null -ne $freeMB) { [math]::Round($freeMB, 1) } else { $null }
    memory_used_percent = $usedPct
    pagefiles = @($page)
    top_processes_by_memory = @($topByMemory)
    top_processes_by_cpu = @($topByCpu)
} | ConvertTo-Json -Compress -Depth 5
`, top)
	return runPwshJSON(ctx, script)
}

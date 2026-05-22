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

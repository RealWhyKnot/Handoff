// SPDX-License-Identifier: GPL-3.0-or-later
//
// Package capabilities holds the per-kind handlers wired into the
// dispatch router. Each file groups related kinds.

package capabilities

import (
	"context"
	"encoding/json"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterSys attaches sys.* handlers to the given router.
func RegisterSys(r *dispatch.Router) {
	r.Register("sys.info", sysInfo)
	r.Register("sys.uptime", sysUptime)
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

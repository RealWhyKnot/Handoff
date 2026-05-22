// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"strconv"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterEvt wires evt.snapshot. Args (all optional):
//
//	channel        string  -- log name (default "System")
//	max_events     int     -- cap on returned rows (default 200, max 5000)
//	since_minutes  int     -- only events newer than this many minutes (default 60)
func RegisterEvt(r *dispatch.Router) {
	r.Register("evt.snapshot", evtSnapshot)
}

func evtSnapshot(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	channel := "System"
	if v, ok := args["channel"]; ok {
		_ = json.Unmarshal(v, &channel)
	}
	maxEvents := 200
	if v, ok := args["max_events"]; ok {
		_ = json.Unmarshal(v, &maxEvents)
	}
	if maxEvents <= 0 {
		maxEvents = 200
	}
	if maxEvents > 5000 {
		maxEvents = 5000
	}
	since := 60
	if v, ok := args["since_minutes"]; ok {
		_ = json.Unmarshal(v, &since)
	}
	if since <= 0 {
		since = 60
	}

	// Whitelist the channel to a small known set to keep the surface
	// predictable and avoid arbitrary log enumeration. Operators can
	// pass the standard channels by name; anything else is rejected.
	allowed := map[string]bool{
		"System": true, "Application": true, "Setup": true, "Security": false,
		"Microsoft-Windows-Kernel-PnP/Configuration": true,
		"Microsoft-Windows-USB-USBHUB3-Analytic":     true,
	}
	if ok := allowed[channel]; !ok {
		return nil, fmt.Errorf("channel %q not in evt.snapshot allowlist", channel)
	}

	script := `
$start = (Get-Date).AddMinutes(-` + strconv.Itoa(since) + `)
Get-WinEvent -FilterHashtable @{ LogName = '` + channel + `'; StartTime = $start } -MaxEvents ` + strconv.Itoa(maxEvents) + ` -ErrorAction SilentlyContinue |
    Select-Object @{n='time';e={$_.TimeCreated.ToUniversalTime().ToString("o")}},LevelDisplayName,Id,ProviderName,Message |
    ConvertTo-Json -Compress -Depth 4
`
	return runPwshJSON(ctx, script)
}

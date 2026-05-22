// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"strconv"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterEvt wires evt.snapshot. Args (all optional):
//
//	channel        string  -- log name (default "System")
//	max_events     int     -- cap on returned rows (default 200, max 5000)
//	since_minutes  int     -- only events newer than this many minutes (default 60)
//
// evt.providers lists the event log channels the host knows about, with their
// record counts. Args:
//
//	name_prefix    string  -- optional case-insensitive name filter
//	max_results    int     -- cap on returned rows (default 400, max 4000)
func RegisterEvt(r *dispatch.Router) {
	r.Register("evt.snapshot", evtSnapshot)
	r.Register("evt.providers", evtProviders)
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

func evtProviders(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var prefix string
	if v, ok := args["name_prefix"]; ok {
		_ = json.Unmarshal(v, &prefix)
	}
	prefix = strings.TrimSpace(prefix)
	if len(prefix) > 128 {
		return nil, fmt.Errorf("evt.providers: name_prefix too long")
	}

	maxResults := 400
	if v, ok := args["max_results"]; ok {
		_ = json.Unmarshal(v, &maxResults)
	}
	if maxResults <= 0 {
		maxResults = 400
	}
	if maxResults > 4000 {
		maxResults = 4000
	}

	script := fmt.Sprintf(`
$prefix = %s
$max = %d
$logs = Get-WinEvent -ListLog * -ErrorAction SilentlyContinue
if ($prefix) {
    $needle = $prefix.ToLowerInvariant()
    $logs = $logs | Where-Object { $_.LogName.ToLowerInvariant().StartsWith($needle) }
}
$logs |
    Sort-Object LogName |
    Select-Object -First $max |
    Select-Object @{n='name';e={$_.LogName}},
                  @{n='enabled';e={[bool]$_.IsEnabled}},
                  @{n='mode';e={[string]$_.LogMode}},
                  @{n='record_count';e={[int64]$_.RecordCount}},
                  @{n='file_size';e={[int64]$_.FileSize}},
                  @{n='owning_provider';e={[string]$_.OwningProviderName}} |
    ConvertTo-Json -Compress -Depth 4
`, psSingleQuote(prefix), maxResults)
	return runPwshJSON(ctx, script)
}

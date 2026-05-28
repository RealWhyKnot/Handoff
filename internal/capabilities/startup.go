// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterStartup wires read-only startup-entry inventory handlers.
func RegisterStartup(r *dispatch.Router) {
	r.Register("startup.list", startupList)
}

func startupMaxResultsArg(args map[string]json.RawMessage) int {
	maxResults := 300
	if v, ok := args["max_results"]; ok {
		_ = json.Unmarshal(v, &maxResults)
	}
	if maxResults <= 0 {
		maxResults = 300
	}
	if maxResults > 2000 {
		maxResults = 2000
	}
	return maxResults
}

func startupList(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	maxResults := startupMaxResultsArg(args)
	script := fmt.Sprintf(`
$max = %d
$entries = Get-CimInstance Win32_StartupCommand -ErrorAction SilentlyContinue |
    Sort-Object Location, Name |
    Select-Object -First $max |
    ForEach-Object {
        [ordered]@{
            name = [string]$_.Name
            command = [string]$_.Command
            location = [string]$_.Location
            user = [string]$_.User
            user_sid = [string]$_.UserSID
            description = [string]$_.Description
        }
    }

[ordered]@{
    max = $max
    count = @($entries).Count
    entries = @($entries)
} | ConvertTo-Json -Compress -Depth 4
`, maxResults)
	return runPwshJSON(ctx, script)
}

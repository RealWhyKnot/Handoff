// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterTask wires the read-only scheduled task capability.
//
// task.list returns scheduled tasks filtered by an optional path prefix
// and state. State must be one of: all, ready, running, disabled, queued,
// unknown. The default is "all". Up to max_results rows are returned with
// trigger summaries for context.
func RegisterTask(r *dispatch.Router) {
	r.RegisterSpec(dispatch.Spec{
		Kind:        "task.list",
		Label:       "Scheduled tasks",
		Description: "List Windows scheduled tasks.",
		Params: []dispatch.Param{
			{Name: "path_prefix", Type: dispatch.ParamString, Description: "Match tasks whose path starts with this."},
			{
				Name:    "state",
				Type:    dispatch.ParamEnum,
				Enum:    []string{"all", "ready", "running", "disabled", "queued", "unknown"},
				Default: "all",
			},
			limitParam(300, 1, 2000),
		},
	}, taskList)
}

func taskList(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var (
		pathPrefix string
		state      string
		maxResults int
	)
	if v, ok := args["path_prefix"]; ok {
		_ = json.Unmarshal(v, &pathPrefix)
	}
	if v, ok := args["state"]; ok {
		_ = json.Unmarshal(v, &state)
	}
	if v, ok := args["limit"]; ok {
		_ = json.Unmarshal(v, &maxResults)
	}

	pathPrefix = strings.TrimSpace(pathPrefix)
	if len(pathPrefix) > 256 {
		return nil, fmt.Errorf("task.list: path_prefix too long")
	}

	state = strings.ToLower(strings.TrimSpace(state))
	if state == "" {
		state = "all"
	}
	if !isValidTaskState(state) {
		return nil, fmt.Errorf("task.list: state must be all, ready, running, disabled, queued, or unknown")
	}

	if maxResults <= 0 {
		maxResults = 300
	}
	if maxResults > 2000 {
		maxResults = 2000
	}

	script := fmt.Sprintf(`
$prefix = %s
$state  = %s
$max    = %d

$tasks = Get-ScheduledTask -ErrorAction SilentlyContinue
if ($prefix) {
    $needle = $prefix.ToLowerInvariant()
    $tasks = $tasks | Where-Object { $_.TaskPath.ToLowerInvariant().StartsWith($needle) }
}
if ($state -ne 'all') {
    $tasks = $tasks | Where-Object { $_.State.ToString().ToLowerInvariant() -eq $state }
}

$results = $tasks |
    Sort-Object TaskPath, TaskName |
    Select-Object -First $max |
    ForEach-Object {
        $info = $null
        try { $info = $_ | Get-ScheduledTaskInfo -ErrorAction SilentlyContinue } catch { $info = $null }
        $triggers = @()
        foreach ($trig in @($_.Triggers)) {
            $triggers += [ordered]@{
                type       = if ($trig.CimClass) { [string]$trig.CimClass.CimClassName } else { 'Unknown' }
                enabled    = [bool]$trig.Enabled
                start      = if ($trig.StartBoundary) { [string]$trig.StartBoundary } else { $null }
            }
        }
        $actions = @()
        foreach ($act in @($_.Actions)) {
            $actions += [ordered]@{
                type     = if ($act.CimClass) { [string]$act.CimClass.CimClassName } else { 'Unknown' }
                execute  = $act.Execute
                arguments = $act.Arguments
            }
        }
        [ordered]@{
            name        = [string]$_.TaskName
            path        = [string]$_.TaskPath
            state       = [string]$_.State
            author      = [string]$_.Author
            description = [string]$_.Description
            last_run    = if ($info -and $info.LastRunTime) { $info.LastRunTime.ToUniversalTime().ToString("o") } else { $null }
            next_run    = if ($info -and $info.NextRunTime) { $info.NextRunTime.ToUniversalTime().ToString("o") } else { $null }
            last_result = if ($info) { [int]$info.LastTaskResult } else { $null }
            triggers    = $triggers
            actions     = $actions
        }
    }

[ordered]@{
    prefix  = $prefix
    state   = $state
    count   = @($results).Count
    max     = $max
    entries = @($results)
} | ConvertTo-Json -Compress -Depth 6
`, psSingleQuote(pathPrefix), psSingleQuote(state), maxResults)
	return runPwshJSON(ctx, script)
}

func isValidTaskState(state string) bool {
	switch state {
	case "all", "ready", "running", "disabled", "queued", "unknown":
		return true
	default:
		return false
	}
}

// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterProc wires proc.* and svc.* handlers.
func RegisterProc(r *dispatch.Router) {
	r.Register("proc.list", procList)
	r.Register("proc.kill", procKill)
	r.Register("proc.find", procFind)
	r.Register("svc.list", svcList)
	r.Register("svc.control", svcControl)
}

func procList(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	// Win32_Process gives the executable path that Get-Process can't always
	// see (when the process is owned by another user); we join the two so
	// the operator gets path + cpu + memory together.
	script := `
$procs = Get-CimInstance Win32_Process | Select-Object ProcessId,Name,ExecutablePath,CommandLine,@{n='WorkingSetMB';e={[math]::Round($_.WorkingSetSize/1MB,1)}},CreationDate
$procs | ConvertTo-Json -Compress -Depth 4
`
	return runPwshJSON(ctx, script)
}

func svcList(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	return runPwshJSON(ctx, `Get-Service | Select-Object Name,DisplayName,Status,StartType | ConvertTo-Json -Compress`)
}

func procKill(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var pid int
	if v, ok := args["pid"]; ok {
		_ = json.Unmarshal(v, &pid)
	}
	if pid <= 0 {
		return nil, fmt.Errorf("proc.kill: 'pid' must be a positive integer")
	}
	if pid == os.Getpid() {
		return nil, fmt.Errorf("proc.kill: refusing to terminate the current handoff process")
	}
	if err := requireRiskConsent(ctx, "proc.kill", fmt.Sprintf("Terminates process ID %d on this computer. Unsaved work in that process can be lost.", pid)); err != nil {
		return nil, err
	}
	script := fmt.Sprintf(`
$targetPid = %d
$proc = Get-Process -Id $targetPid -ErrorAction Stop
$name = $proc.ProcessName
$path = $proc.Path
Stop-Process -Id $targetPid -Force -ErrorAction Stop
[ordered]@{
    pid = $targetPid
    name = $name
    path = $path
    killed = $true
} | ConvertTo-Json -Compress
`, pid)
	return runPwshJSON(ctx, script)
}

func procFind(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var (
		query      string
		maxResults int
	)
	if v, ok := args["query"]; ok {
		_ = json.Unmarshal(v, &query)
	}
	query = strings.TrimSpace(query)
	if query == "" {
		return nil, fmt.Errorf("proc.find: 'query' is required")
	}
	if len(query) > 128 {
		return nil, fmt.Errorf("proc.find: 'query' is too long")
	}
	if v, ok := args["max_results"]; ok {
		_ = json.Unmarshal(v, &maxResults)
	}
	if maxResults <= 0 {
		maxResults = 100
	}
	if maxResults > 500 {
		maxResults = 500
	}

	script := fmt.Sprintf(`
$q = [regex]::Escape([string]%s)
Get-CimInstance Win32_Process | Where-Object {
    $_.Name -match $q -or $_.ExecutablePath -match $q -or $_.CommandLine -match $q
} | Select-Object ProcessId,Name,ExecutablePath,CommandLine,ParentProcessId,SessionId,@{n='WorkingSetMB';e={[math]::Round($_.WorkingSetSize/1MB,1)}} |
    Select-Object -First %d |
    ConvertTo-Json -Compress -Depth 4
`, psSingleQuote(query), maxResults)
	return runPwshJSON(ctx, script)
}

func svcControl(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var (
		name   string
		action string
	)
	if v, ok := args["name"]; ok {
		_ = json.Unmarshal(v, &name)
	}
	if v, ok := args["action"]; ok {
		_ = json.Unmarshal(v, &action)
	}
	name = strings.TrimSpace(name)
	action = strings.ToLower(strings.TrimSpace(action))
	if name == "" {
		return nil, fmt.Errorf("svc.control: 'name' is required")
	}
	switch action {
	case "start", "stop", "restart":
	default:
		return nil, fmt.Errorf("svc.control: action must be start, stop, or restart")
	}
	if err := requireRiskConsent(ctx, "svc.control", fmt.Sprintf("Runs '%s' against Windows service %q. This can interrupt software or device functionality.", action, name)); err != nil {
		return nil, err
	}
	script := fmt.Sprintf(`
$name = %s
$action = %s
$before = Get-Service -Name $name -ErrorAction Stop
switch ($action) {
    'start' { Start-Service -Name $name -ErrorAction Stop }
    'stop' { Stop-Service -Name $name -Force -ErrorAction Stop }
    'restart' { Restart-Service -Name $name -Force -ErrorAction Stop }
}
$after = Get-Service -Name $name -ErrorAction Stop
[ordered]@{
    name = $after.Name
    display_name = $after.DisplayName
    action = $action
    before = $before.Status.ToString()
    after = $after.Status.ToString()
} | ConvertTo-Json -Compress
`, psSingleQuote(name), psSingleQuote(action))
	return runPwshJSON(ctx, script)
}

func psSingleQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", "''") + "'"
}

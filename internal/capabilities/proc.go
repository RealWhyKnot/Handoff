// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterProc wires proc.* and svc.* read-only list handlers.
func RegisterProc(r *dispatch.Router) {
	r.Register("proc.list", procList)
	r.Register("svc.list", svcList)
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

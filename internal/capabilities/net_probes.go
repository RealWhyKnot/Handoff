// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"strconv"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterNetProbes wires net.ping and net.trace. Both produce outbound
// traffic but only to the target the operator names; the destination is
// validated as a hostname or IP before any traffic is generated.
func RegisterNetProbes(r *dispatch.Router) {
	r.Register("net.ping", netPing)
	r.Register("net.trace", netTrace)
}

func validTarget(s string) bool {
	if s == "" {
		return false
	}
	if ip := net.ParseIP(s); ip != nil {
		return true
	}
	// Hostname: very loose, just refuse spaces and shell metacharacters.
	for _, r := range s {
		if r < 0x20 || r == ' ' || r == '"' || r == '\'' || r == '&' || r == '|' || r == ';' || r == '`' || r == '$' || r == '<' || r == '>' {
			return false
		}
	}
	return len(s) <= 253
}

func netPing(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var target string
	if v, ok := args["target"]; ok {
		_ = json.Unmarshal(v, &target)
	}
	count := 4
	if v, ok := args["count"]; ok {
		_ = json.Unmarshal(v, &count)
	}
	if count <= 0 || count > 10 {
		count = 4
	}
	if !validTarget(target) {
		return nil, fmt.Errorf("net.ping: invalid 'target'")
	}
	script := `Test-Connection -ComputerName '` + target + `' -Count ` + strconv.Itoa(count) + ` -ErrorAction SilentlyContinue |
		Select-Object Address,@{n='ResponseTimeMs';e={$_.ResponseTime}},StatusCode |
		ConvertTo-Json -Compress`
	return runPwshJSON(ctx, script)
}

func netTrace(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var target string
	if v, ok := args["target"]; ok {
		_ = json.Unmarshal(v, &target)
	}
	if !validTarget(target) {
		return nil, fmt.Errorf("net.trace: invalid 'target'")
	}
	// Test-NetConnection -TraceRoute can take 10-30s. The ctx deadline
	// handed by the dispatcher is the only bound; callers should not
	// trace through more than a few hops on slow paths.
	script := `Test-NetConnection -ComputerName '` + target + `' -TraceRoute -InformationLevel Detailed -WarningAction SilentlyContinue |
		Select-Object ComputerName,RemoteAddress,PingSucceeded,PingReplyDetails,TraceRoute |
		ConvertTo-Json -Compress -Depth 4`
	return runPwshJSON(ctx, script)
}

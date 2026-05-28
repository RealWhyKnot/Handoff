// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"strconv"
	"strings"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterNetProbes wires net.ping and net.trace. Both produce outbound
// traffic but only to the target the operator names; the destination is
// validated as a hostname or IP before any traffic is generated.
func RegisterNetProbes(r *dispatch.Router) {
	r.Register("net.ping", netPing)
	r.Register("net.trace", netTrace)
	r.Register("net.tcp-test", netTCPTest)
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

type tcpTestOptions struct {
	target    string
	port      int
	timeoutMS int
}

func parseTCPTestOptions(args map[string]json.RawMessage) (tcpTestOptions, error) {
	opts := tcpTestOptions{timeoutMS: 5000}
	if v, ok := args["target"]; ok {
		_ = json.Unmarshal(v, &opts.target)
	}
	if v, ok := args["port"]; ok {
		_ = json.Unmarshal(v, &opts.port)
	}
	if v, ok := args["timeout_ms"]; ok {
		_ = json.Unmarshal(v, &opts.timeoutMS)
	}
	opts.target = strings.TrimSpace(opts.target)
	if !validTarget(opts.target) {
		return opts, fmt.Errorf("net.tcp-test: invalid 'target'")
	}
	if opts.port <= 0 || opts.port > 65535 {
		return opts, fmt.Errorf("net.tcp-test: port must be 1-65535")
	}
	if opts.timeoutMS < 1000 {
		opts.timeoutMS = 1000
	}
	if opts.timeoutMS > 30000 {
		opts.timeoutMS = 30000
	}
	return opts, nil
}

func netTCPTest(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	opts, err := parseTCPTestOptions(args)
	if err != nil {
		return nil, err
	}

	start := time.Now()
	timeout := time.Duration(opts.timeoutMS) * time.Millisecond
	result := map[string]interface{}{
		"target":             opts.target,
		"port":               opts.port,
		"timeout_ms":         opts.timeoutMS,
		"tcp_test_succeeded": false,
	}

	lookupCtx, lookupCancel := context.WithTimeout(ctx, timeout)
	addrs, lookupErr := net.DefaultResolver.LookupIPAddr(lookupCtx, opts.target)
	lookupCancel()
	resolved := make([]string, 0, len(addrs))
	for _, addr := range addrs {
		resolved = append(resolved, addr.IP.String())
	}
	result["resolved_addresses"] = resolved
	if lookupErr != nil {
		result["error"] = lookupErr.Error()
		result["elapsed_ms"] = time.Since(start).Milliseconds()
		return result, nil
	}
	if len(resolved) > 0 {
		result["remote_address"] = net.JoinHostPort(resolved[0], strconv.Itoa(opts.port))
	}

	dialCtx, dialCancel := context.WithTimeout(ctx, timeout)
	conn, dialErr := (&net.Dialer{Timeout: timeout}).DialContext(dialCtx, "tcp", net.JoinHostPort(opts.target, strconv.Itoa(opts.port)))
	dialCancel()
	if dialErr != nil {
		result["error"] = dialErr.Error()
		result["elapsed_ms"] = time.Since(start).Milliseconds()
		return result, nil
	}
	defer conn.Close()

	result["tcp_test_succeeded"] = true
	result["remote_address"] = conn.RemoteAddr().String()
	result["local_address"] = conn.LocalAddr().String()
	result["elapsed_ms"] = time.Since(start).Milliseconds()
	return result, nil
}

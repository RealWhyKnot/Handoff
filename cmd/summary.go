// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"encoding/json"
	"fmt"
	"sort"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// summaryKeys are the arguments worth showing the host. The point is to answer
// "what is happening to my computer", so it favours targets over tuning knobs.
var summaryKeys = []string{
	"path", "target", "url", "host", "name", "action", "pid", "query",
	"script", "channel", "serial", "local_port", "key", "uf2_path", "out_path",
}

// bulkKeys carry payloads, not information. Showing their contents would bury
// the line and copy the payload into the console.
var bulkKeys = map[string]bool{
	"data_base64":  true,
	"body_base64":  true,
	"base64":       true,
	"image_base64": true,
	"zip_base64":   true,
}

const (
	maxSummaryValue = 120
	maxSummaryTotal = 200
)

// summarizeArgs renders the interesting arguments of a command on one line.
func summarizeArgs(args map[string]json.RawMessage) string {
	if len(args) == 0 {
		return ""
	}

	seen := map[string]bool{}
	var parts []string

	add := func(key string) {
		raw, ok := args[key]
		if !ok || seen[key] {
			return
		}
		seen[key] = true
		if bulkKeys[key] {
			parts = append(parts, fmt.Sprintf("%s=<%d bytes>", key, len(raw)))
			return
		}
		parts = append(parts, key+"="+summaryValue(raw))
	}

	for _, k := range summaryKeys {
		add(k)
	}
	if len(parts) == 0 {
		rest := make([]string, 0, len(args))
		for k := range args {
			rest = append(rest, k)
		}
		sort.Strings(rest)
		for _, k := range rest {
			add(k)
			if len(parts) >= 2 {
				break
			}
		}
	}

	out := strings.Join(parts, " ")
	if len(out) > maxSummaryTotal {
		out = out[:maxSummaryTotal] + "..."
	}
	return out
}

func summaryValue(raw json.RawMessage) string {
	var v interface{}
	if err := json.Unmarshal(raw, &v); err != nil {
		return "?"
	}
	var s string
	switch t := v.(type) {
	case string:
		s = t
	default:
		b, err := json.Marshal(t)
		if err != nil {
			return "?"
		}
		s = string(b)
	}
	s = strings.ReplaceAll(s, "\n", " ")
	s = strings.ReplaceAll(s, "\r", "")
	if len(s) > maxSummaryValue {
		s = s[:maxSummaryValue] + "..."
	}
	if strings.ContainsAny(s, " \t") {
		return fmt.Sprintf("%q", s)
	}
	return s
}

// summarizeResult describes the outcome in the terms of whatever ran, so the
// host sees a row count or an exit code rather than only ok/elapsed.
func summarizeResult(out dispatch.Outcome) string {
	if !out.OK {
		msg := strings.ReplaceAll(out.Error, "\n", " ")
		if len(msg) > maxSummaryValue {
			msg = msg[:maxSummaryValue] + "..."
		}
		return msg
	}

	switch res := out.Result.(type) {
	case map[string]interface{}:
		if code, ok := res["exit_code"]; ok {
			stdout, _ := res["stdout"].(string)
			return fmt.Sprintf("exit=%v stdout=%d bytes", code, len(stdout))
		}
		if n, ok := res["count"]; ok {
			return fmt.Sprintf("%v row(s)", n)
		}
		if size, ok := res["size"]; ok {
			return fmt.Sprintf("%v bytes", size)
		}
		if entries, ok := res["entries"].([]interface{}); ok {
			return fmt.Sprintf("%d row(s)", len(entries))
		}
	case []interface{}:
		return fmt.Sprintf("%d row(s)", len(res))
	}
	return ""
}

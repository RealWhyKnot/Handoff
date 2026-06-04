// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"sync"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterPs wires ps.exec. The first risky command in a session prompts the
// host for consent; a yes allows risky commands until the session exits.
//
// Executions are rate-limited to 10 per rolling minute per session to slow
// runaway loops.
func RegisterPs(r *dispatch.Router) {
	r.Register("ps.exec", psExec())
}

var (
	psMu      sync.Mutex
	psHistory []time.Time
	psLimit   = 10
	psWindow  = time.Minute
)

const psScriptCap = 64 * 1024

func psExec() dispatch.Handler {
	return func(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
		var script string
		if v, ok := args["script"]; ok {
			_ = json.Unmarshal(v, &script)
		}
		script = strings.TrimSpace(script)
		if script == "" {
			return nil, fmt.Errorf("ps.exec: 'script' is required")
		}
		if len(script) > psScriptCap {
			return nil, fmt.Errorf("ps.exec: script is %d bytes; cap is %d", len(script), psScriptCap)
		}
		if err := requireRiskConsent(ctx, "ps.exec", "Runs arbitrary PowerShell code on this computer. The script can read, change, or delete files and can start programs as the current user."); err != nil {
			return nil, err
		}

		// Rate limit.
		now := time.Now()
		psMu.Lock()
		cutoff := now.Add(-psWindow)
		kept := psHistory[:0]
		for _, t := range psHistory {
			if t.After(cutoff) {
				kept = append(kept, t)
			}
		}
		psHistory = kept
		if len(psHistory) >= psLimit {
			psMu.Unlock()
			return nil, fmt.Errorf("ps.exec: rate limit reached (%d in last %s)", psLimit, psWindow)
		}
		psHistory = append(psHistory, now)
		psMu.Unlock()

		// Run as a single -Command invocation; capture stdout and stderr.
		out, err := runPwsh(ctx, script)
		if err != nil {
			return map[string]interface{}{
				"ok":     false,
				"stdout": string(out),
				"error":  err.Error(),
			}, nil
		}
		return map[string]interface{}{
			"ok":     true,
			"stdout": string(out),
		}, nil
	}
}

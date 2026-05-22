// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterPs wires ps.exec. The kind is disabled by default; the host
// has to opt in by setting HANDOFF_ALLOW_PSEXEC=1 in the environment
// before running `handoff new`. The handler is always registered so the
// operator gets a clear "disabled" message rather than "unknown kind".
//
// When enabled, executions are rate-limited to 10 per rolling minute
// per session to slow runaway loops.
func RegisterPs(r *dispatch.Router) {
	r.Register("ps.exec", psExec())
}

var (
	psMu       sync.Mutex
	psHistory  []time.Time
	psLimit    = 10
	psWindow   = time.Minute
)

func psExec() dispatch.Handler {
	return func(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
		if os.Getenv("HANDOFF_ALLOW_PSEXEC") != "1" {
			return map[string]interface{}{
				"ok":     false,
				"reason": "ps.exec is disabled; rerun handoff with HANDOFF_ALLOW_PSEXEC=1 to enable",
			}, nil
		}

		var script string
		if v, ok := args["script"]; ok {
			_ = json.Unmarshal(v, &script)
		}
		script = strings.TrimSpace(script)
		if script == "" {
			return nil, fmt.Errorf("ps.exec: 'script' is required")
		}
		if len(script) > 16*1024 {
			return nil, fmt.Errorf("ps.exec: script is %d bytes; cap is 16384", len(script))
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

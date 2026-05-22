// SPDX-License-Identifier: GPL-3.0-or-later
//
// Shared helper for the PowerShell-backed capabilities. Most kinds
// shell out to powershell.exe (the Windows-built-in 5.1, not pwsh 7)
// and parse the resulting JSON.

package capabilities

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os/exec"
	"strings"
)

// runPwsh executes a PowerShell script and returns stdout + stderr.
// Non-zero exits propagate as errors with stderr attached.
func runPwsh(ctx context.Context, script string) ([]byte, error) {
	cmd := exec.CommandContext(ctx, "powershell.exe",
		"-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
		"-Command", script)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return stdout.Bytes(), fmt.Errorf("powershell: %v: %s", err, strings.TrimSpace(stderr.String()))
	}
	return stdout.Bytes(), nil
}

// runPwshJSON runs the script, expects JSON on stdout, and returns the
// parsed value (anything json.RawMessage can hold -- objects, arrays,
// scalars). Empty stdout produces nil.
func runPwshJSON(ctx context.Context, script string) (interface{}, error) {
	out, err := runPwsh(ctx, script)
	if err != nil {
		return nil, err
	}
	out = bytes.TrimSpace(out)
	if len(out) == 0 {
		return nil, nil
	}
	var v interface{}
	if err := json.Unmarshal(out, &v); err != nil {
		// Some cmdlets emit non-JSON warnings on the first line; try to
		// salvage the JSON portion if the rest of the buffer parses.
		if i := bytes.IndexAny(out, "[{"); i > 0 {
			if err2 := json.Unmarshal(out[i:], &v); err2 == nil {
				return v, nil
			}
		}
		return nil, fmt.Errorf("decode powershell json: %w (got %d bytes)", err, len(out))
	}
	return v, nil
}

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

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// pwshResult carries everything a caller needs to report a shell-out honestly:
// both streams and the numeric exit code, rather than one string with stderr
// concatenated into it.
type pwshResult struct {
	Stdout   []byte
	Stderr   []byte
	ExitCode int
}

// runPwshFull executes a PowerShell script and reports both streams plus the
// exit code. A non-zero exit is returned as a *dispatch.Failure so the command
// envelope's ok flag stays truthful.
func runPwshFull(ctx context.Context, script string) (pwshResult, error) {
	cmd := exec.CommandContext(ctx, "powershell.exe",
		"-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
		"-Command", script)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	err := cmd.Run()
	res := pwshResult{Stdout: stdout.Bytes(), Stderr: stderr.Bytes(), ExitCode: cmd.ProcessState.ExitCode()}
	if err == nil {
		return res, nil
	}

	msg := strings.TrimSpace(string(res.Stderr))
	if msg == "" {
		msg = err.Error()
	}
	code := res.ExitCode
	return res, &dispatch.Failure{
		Message:  fmt.Sprintf("powershell exited %d: %s", code, msg),
		ExitCode: &code,
		Stdout:   string(res.Stdout),
		Stderr:   string(res.Stderr),
	}
}

// runPwsh executes a PowerShell script and returns stdout.
// Non-zero exits propagate as errors with stderr attached.
func runPwsh(ctx context.Context, script string) ([]byte, error) {
	res, err := runPwshFull(ctx, script)
	return res.Stdout, err
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
		// An empty result set and a cmdlet that printed nothing are the same
		// thing to the caller; both are "no rows", never a null.
		return []interface{}{}, nil
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

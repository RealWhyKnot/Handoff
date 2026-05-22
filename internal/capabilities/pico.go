// SPDX-License-Identifier: GPL-3.0-or-later
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

// RegisterPico wires the pico.* handlers. v0.1 shells out to the
// system-installed picotool; v0.2 will embed it via go:embed so the
// host doesn't need to install anything beyond handoff.exe.
func RegisterPico(r *dispatch.Router) {
	r.Register("pico.list", picoList)
	r.Register("pico.info", picoInfo)
	r.Register("pico.bootsel", picoBootsel)
	r.Register("pico.flash", picoFlash)
	r.Register("pico.save", picoSave)
	r.Register("pico.reset", picoReset)
}

func picotoolPath() (string, error) {
	p, err := exec.LookPath("picotool")
	if err == nil {
		return p, nil
	}
	p, err = exec.LookPath("picotool.exe")
	if err == nil {
		return p, nil
	}
	return "", fmt.Errorf("picotool not on PATH (install from raspberrypi/pico-sdk-tools or `winget install raspberrypi.picotool`)")
}

func runPicotool(ctx context.Context, args ...string) (stdout, stderr string, err error) {
	pt, err := picotoolPath()
	if err != nil {
		return "", "", err
	}
	cmd := exec.CommandContext(ctx, pt, args...)
	var out, errb bytes.Buffer
	cmd.Stdout = &out
	cmd.Stderr = &errb
	runErr := cmd.Run()
	return out.String(), errb.String(), runErr
}

func picoList(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	out, errb, err := runPicotool(ctx, "info", "-a")
	if err != nil {
		return map[string]interface{}{
			"ok":      false,
			"reason":  err.Error(),
			"stderr":  strings.TrimSpace(errb),
			"stdout":  strings.TrimSpace(out),
		}, nil
	}
	return map[string]interface{}{
		"ok":     true,
		"raw":    out,
		"format": "picotool-info-text",
	}, nil
}

func picoInfo(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	cmdArgs := []string{"info", "-a", "-m", "-d", "-l"}
	if v, ok := args["serial"]; ok {
		var s string
		_ = json.Unmarshal(v, &s)
		if s != "" {
			cmdArgs = append(cmdArgs, "--id", s)
		}
	}
	out, errb, err := runPicotool(ctx, cmdArgs...)
	if err != nil {
		return map[string]interface{}{"ok": false, "reason": err.Error(), "stderr": strings.TrimSpace(errb)}, nil
	}
	return map[string]interface{}{"ok": true, "raw": out}, nil
}

func picoBootsel(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	out, errb, err := runPicotool(ctx, "reboot", "-f", "-u")
	if err != nil {
		return map[string]interface{}{
			"ok":     false,
			"reason": err.Error(),
			"stderr": strings.TrimSpace(errb),
			"stdout": strings.TrimSpace(out),
		}, nil
	}
	return map[string]interface{}{"ok": true, "raw": out}, nil
}

func picoFlash(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var uf2 string
	if v, ok := args["uf2_path"]; ok {
		_ = json.Unmarshal(v, &uf2)
	}
	if uf2 == "" {
		return nil, fmt.Errorf("pico.flash: 'uf2_path' is required")
	}
	out, errb, err := runPicotool(ctx, "load", "-fx", uf2)
	if err != nil {
		return map[string]interface{}{"ok": false, "reason": err.Error(), "stderr": strings.TrimSpace(errb), "stdout": strings.TrimSpace(out)}, nil
	}
	return map[string]interface{}{"ok": true, "uf2": uf2, "raw": out}, nil
}

func picoSave(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var out string
	if v, ok := args["out_path"]; ok {
		_ = json.Unmarshal(v, &out)
	}
	if out == "" {
		return nil, fmt.Errorf("pico.save: 'out_path' is required")
	}
	stdout, errb, err := runPicotool(ctx, "save", "-a", out)
	if err != nil {
		return map[string]interface{}{"ok": false, "reason": err.Error(), "stderr": strings.TrimSpace(errb), "stdout": strings.TrimSpace(stdout)}, nil
	}
	return map[string]interface{}{"ok": true, "out_path": out, "raw": stdout}, nil
}

func picoReset(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	out, errb, err := runPicotool(ctx, "reboot")
	if err != nil {
		return map[string]interface{}{"ok": false, "reason": err.Error(), "stderr": strings.TrimSpace(errb), "stdout": strings.TrimSpace(out)}, nil
	}
	return map[string]interface{}{"ok": true, "raw": out}, nil
}

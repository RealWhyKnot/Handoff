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
	"github.com/RealWhyKnot/Handoff/internal/picotool"
)

// RegisterPico wires the pico.* handlers. v0.1 shells out to the
// system-installed picotool; v0.2 will embed it via go:embed so the
// host doesn't need to install anything beyond handoff.exe.
func RegisterPico(r *dispatch.Router) {
	r.Register("pico.list", picoList)
	r.Register("pico.info", picoInfo)
	r.Register("pico.bootsel", picoBootsel)
	r.Register("pico.flash", picoFlash)
	r.Register("pico.verify", picoVerify)
	r.Register("pico.save", picoSave)
	r.Register("pico.reset", picoReset)
}

func picotoolPath() (string, error) {
	return picotool.Path()
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

// picoStringArg pulls a string off the JSON args map and trims it.
// Missing keys, wrong types, and whitespace-only values all collapse
// to the empty string so callers can do a single `if s == ""` check.
func picoStringArg(args map[string]json.RawMessage, key string) string {
	v, ok := args[key]
	if !ok {
		return ""
	}
	var s string
	_ = json.Unmarshal(v, &s)
	return strings.TrimSpace(s)
}

// withSerial appends `--id <ser>` to a picotool arg slice when a
// serial number was supplied. Pins a command to a specific board
// when multiple Picos are connected to the same host.
func withSerial(args []string, serial string) []string {
	if serial == "" {
		return args
	}
	return append(args, "--id", serial)
}

// picoFamilies are the picotool `--family` identifiers we accept on
// load/verify. Validated up front so a typo doesn't reach picotool.
var picoFamilies = map[string]struct{}{
	"rp2040":        {},
	"rp2350-arm-s":  {},
	"rp2350-arm-ns": {},
	"rp2350-riscv":  {},
	"absolute":      {},
	"data":          {},
}

// picoCPUs are the architectures picotool can switch to on RP2350
// via `reboot -c`. RP2040 has only one core arch and will reject it.
var picoCPUs = map[string]struct{}{
	"arm":   {},
	"riscv": {},
}

func picoList(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	out, errb, err := runPicotool(ctx, "info", "-a")
	if err != nil {
		return map[string]interface{}{
			"ok":     false,
			"reason": err.Error(),
			"stderr": strings.TrimSpace(errb),
			"stdout": strings.TrimSpace(out),
		}, nil
	}
	return map[string]interface{}{
		"ok":     true,
		"raw":    out,
		"format": "picotool-info-text",
	}, nil
}

func picoInfo(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	cmdArgs := withSerial([]string{"info", "-a", "-m", "-d", "-l"}, picoStringArg(args, "serial"))
	out, errb, err := runPicotool(ctx, cmdArgs...)
	if err != nil {
		return map[string]interface{}{"ok": false, "reason": err.Error(), "stderr": strings.TrimSpace(errb)}, nil
	}
	return map[string]interface{}{"ok": true, "raw": out}, nil
}

func picoBootselArgs(args map[string]json.RawMessage) ([]string, error) {
	cmdArgs := []string{"reboot", "-f", "-u"}
	if cpu := picoStringArg(args, "cpu"); cpu != "" {
		if _, ok := picoCPUs[cpu]; !ok {
			return nil, fmt.Errorf("pico.bootsel: cpu must be 'arm' or 'riscv'")
		}
		cmdArgs = append(cmdArgs, "-c", cpu)
	}
	if part := picoStringArg(args, "partition"); part != "" {
		cmdArgs = append(cmdArgs, "-g", part)
	}
	return withSerial(cmdArgs, picoStringArg(args, "serial")), nil
}

func picoBootsel(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	cmdArgs, err := picoBootselArgs(args)
	if err != nil {
		return nil, err
	}
	if err := requireRiskConsent(ctx, "pico.bootsel", "Reboots an attached Pico into BOOTSEL mode. Connected software may lose its current device connection."); err != nil {
		return nil, err
	}
	out, errb, err := runPicotool(ctx, cmdArgs...)
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

func picoFlashArgs(args map[string]json.RawMessage) (cmdArgs []string, uf2 string, err error) {
	uf2 = picoStringArg(args, "uf2_path")
	if uf2 == "" {
		return nil, "", fmt.Errorf("pico.flash: 'uf2_path' is required")
	}
	cmdArgs = []string{"load", "-fx"}
	if family := picoStringArg(args, "family"); family != "" {
		if _, ok := picoFamilies[family]; !ok {
			return nil, "", fmt.Errorf("pico.flash: unknown family %q", family)
		}
		cmdArgs = append(cmdArgs, "--family", family)
	}
	cmdArgs = append(cmdArgs, uf2)
	return withSerial(cmdArgs, picoStringArg(args, "serial")), uf2, nil
}

func picoFlash(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	cmdArgs, uf2, err := picoFlashArgs(args)
	if err != nil {
		return nil, err
	}
	if err := requireRiskConsent(ctx, "pico.flash", "Flashes firmware to an attached Pico from a UF2 file. Bad firmware can make the device stop working until it is reflashed."); err != nil {
		return nil, err
	}
	out, errb, err := runPicotool(ctx, cmdArgs...)
	if err != nil {
		return map[string]interface{}{"ok": false, "reason": err.Error(), "stderr": strings.TrimSpace(errb), "stdout": strings.TrimSpace(out)}, nil
	}
	return map[string]interface{}{"ok": true, "uf2": uf2, "raw": out}, nil
}

func picoVerifyArgs(args map[string]json.RawMessage) (cmdArgs []string, path string, err error) {
	path = picoStringArg(args, "file_path")
	if path == "" {
		return nil, "", fmt.Errorf("pico.verify: 'file_path' is required")
	}
	cmdArgs = []string{"verify", "-f"}
	if family := picoStringArg(args, "family"); family != "" {
		if _, ok := picoFamilies[family]; !ok {
			return nil, "", fmt.Errorf("pico.verify: unknown family %q", family)
		}
		cmdArgs = append(cmdArgs, "--family", family)
	}
	cmdArgs = append(cmdArgs, path)
	return withSerial(cmdArgs, picoStringArg(args, "serial")), path, nil
}

// picoVerify reads back flash from the device and compares it byte
// for byte to a UF2/ELF/BIN on disk. Read-only on the device, so no
// risk-consent gate -- the host is not changed by a verify.
func picoVerify(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	cmdArgs, path, err := picoVerifyArgs(args)
	if err != nil {
		return nil, err
	}
	out, errb, runErr := runPicotool(ctx, cmdArgs...)
	if runErr != nil {
		return map[string]interface{}{
			"ok":        false,
			"reason":    runErr.Error(),
			"stderr":    strings.TrimSpace(errb),
			"stdout":    strings.TrimSpace(out),
			"file_path": path,
		}, nil
	}
	return map[string]interface{}{"ok": true, "file_path": path, "raw": out}, nil
}

func picoSave(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	out := picoStringArg(args, "out_path")
	if out == "" {
		return nil, fmt.Errorf("pico.save: 'out_path' is required")
	}
	if err := requireRiskConsent(ctx, "pico.save", "Writes a Pico flash dump to a file on this computer. Existing files may be replaced by picotool behavior."); err != nil {
		return nil, err
	}
	cmdArgs := withSerial([]string{"save", "-a", out}, picoStringArg(args, "serial"))
	stdout, errb, err := runPicotool(ctx, cmdArgs...)
	if err != nil {
		return map[string]interface{}{"ok": false, "reason": err.Error(), "stderr": strings.TrimSpace(errb), "stdout": strings.TrimSpace(stdout)}, nil
	}
	return map[string]interface{}{"ok": true, "out_path": out, "raw": stdout}, nil
}

func picoResetArgs(args map[string]json.RawMessage) ([]string, error) {
	cmdArgs := []string{"reboot"}
	if cpu := picoStringArg(args, "cpu"); cpu != "" {
		if _, ok := picoCPUs[cpu]; !ok {
			return nil, fmt.Errorf("pico.reset: cpu must be 'arm' or 'riscv'")
		}
		cmdArgs = append(cmdArgs, "-c", cpu)
	}
	if part := picoStringArg(args, "partition"); part != "" {
		cmdArgs = append(cmdArgs, "-g", part)
	}
	return withSerial(cmdArgs, picoStringArg(args, "serial")), nil
}

func picoReset(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	cmdArgs, err := picoResetArgs(args)
	if err != nil {
		return nil, err
	}
	if err := requireRiskConsent(ctx, "pico.reset", "Reboots an attached Pico. Connected software may lose its current device connection."); err != nil {
		return nil, err
	}
	out, errb, err := runPicotool(ctx, cmdArgs...)
	if err != nil {
		return map[string]interface{}{"ok": false, "reason": err.Error(), "stderr": strings.TrimSpace(errb), "stdout": strings.TrimSpace(out)}, nil
	}
	return map[string]interface{}{"ok": true, "raw": out}, nil
}

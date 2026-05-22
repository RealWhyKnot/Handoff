// SPDX-License-Identifier: GPL-3.0-or-later
package picotool

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
)

// Path returns a path to a picotool.exe. Priority:
//  1. The embedded binary, extracted to %TEMP%\handoff\picotool.exe
//     on first use and reused thereafter.
//  2. The system PATH (`picotool` or `picotool.exe`).
//
// Errors only if neither is available.
func Path() (string, error) {
	if p, err := extractOnce(); err == nil {
		return p, nil
	}
	if p, err := exec.LookPath("picotool"); err == nil {
		return p, nil
	}
	if p, err := exec.LookPath("picotool.exe"); err == nil {
		return p, nil
	}
	return "", errors.New("picotool not embedded and not on PATH (winget install raspberrypi.picotool)")
}

var (
	extractMu      sync.Mutex
	extractedPath  string
	extractedError error
)

func extractOnce() (string, error) {
	extractMu.Lock()
	defer extractMu.Unlock()

	if extractedPath != "" {
		return extractedPath, nil
	}
	if extractedError != nil {
		return "", extractedError
	}
	if !Embedded() {
		extractedError = errors.New("not embedded")
		return "", extractedError
	}

	dir := filepath.Join(os.TempDir(), "handoff")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		extractedError = err
		return "", err
	}

	// Hash-named so a refresh on a new handoff version produces a new
	// path and avoids the "old picotool stuck on disk" failure mode.
	sum := sha256.Sum256(data)
	name := fmt.Sprintf("picotool-%s.exe", hex.EncodeToString(sum[:6]))
	out := filepath.Join(dir, name)

	if _, err := os.Stat(out); err == nil {
		extractedPath = out
		return out, nil
	}
	tmp := out + ".part"
	if err := os.WriteFile(tmp, data, 0o755); err != nil {
		extractedError = err
		return "", err
	}
	if err := os.Rename(tmp, out); err != nil {
		// Race with a sibling extraction is fine; the final file is
		// byte-identical because the source is the same constant.
		_ = os.Remove(tmp)
		if _, err2 := os.Stat(out); err2 != nil {
			extractedError = err
			return "", err
		}
	}
	extractedPath = out
	return out, nil
}

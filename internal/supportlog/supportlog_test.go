// SPDX-License-Identifier: GPL-3.0-or-later
package supportlog

import (
	"path/filepath"
	"testing"
)

func TestPathForExecutableWritesBesideExe(t *testing.T) {
	got := PathForExecutable(filepath.Join("D:", "Tools", "handoff.exe"))
	want := filepath.Join("D:", "Tools", "handoff.log")
	if got != want {
		t.Fatalf("PathForExecutable() = %q, want %q", got, want)
	}
}

func TestPathForExecutableUsesExecutableBaseName(t *testing.T) {
	got := PathForExecutable(filepath.Join("C:", "Temp", "handoff-preview.exe"))
	want := filepath.Join("C:", "Temp", "handoff-preview.log")
	if got != want {
		t.Fatalf("PathForExecutable() = %q, want %q", got, want)
	}
}

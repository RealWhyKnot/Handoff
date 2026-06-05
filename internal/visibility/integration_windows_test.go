// SPDX-License-Identifier: GPL-3.0-or-later
//go:build windows

package visibility

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
)

// TestCollectParents_IncludesSelf verifies the toolhelp snapshot syscall
// plumbing. Our own PID must show up in the snapshot with a non-zero
// parent.
func TestCollectParents_IncludesSelf(t *testing.T) {
	parents, err := collectParents()
	if err != nil {
		t.Fatalf("collectParents: %v", err)
	}
	self := uint32(os.Getpid())
	parent, ok := parents[self]
	if !ok {
		t.Fatalf("snapshot does not contain our own PID %d", self)
	}
	if parent == 0 {
		t.Errorf("our parent PID is 0; expected a real PPID")
	}
	if len(parents) < 3 {
		t.Errorf("snapshot has %d entries; expected >= 3 on a real Windows session", len(parents))
	}
}

// TestEnumWindows_PopulatesCollector verifies the EnumWindows callback
// plumbing. On any normal desktop session it returns at least one
// top-level window; we skip rather than fail on session-0 / headless
// environments where 0 is plausible.
func TestEnumWindows_PopulatesCollector(t *testing.T) {
	enumCollector = enumCollector[:0]
	enumWindowsProc.Call(enumProcCallback, 0)
	if len(enumCollector) == 0 {
		t.Skip("EnumWindows returned 0 windows; likely a headless or session-0 environment")
	}
}

// TestPerformCheck_PassesInDevEnvironment confirms the full check returns
// ok=true when run from an interactive session with a visible ancestor
// window. Auto-skips on CI.
func TestPerformCheck_PassesInDevEnvironment(t *testing.T) {
	if os.Getenv("CI") != "" {
		t.Skip("skipping live visibility check on CI")
	}
	res := performCheck()
	if !res.ok {
		t.Errorf("performCheck failed in dev environment: %s", res.reason)
	}
}

// TestProbe_SurvivesWithVisibleAncestor builds the probe binary and runs
// it inheriting our environment. Probe should heartbeat and exit cleanly.
// Skips if the current environment has no visible ancestor at all (the
// kill switch would correctly fire there).
func TestProbe_SurvivesWithVisibleAncestor(t *testing.T) {
	if !performCheck().ok {
		t.Skip("no visible ancestor in this environment; can't test survival here")
	}
	probe := buildProbe(t)

	out, err := exec.Command(probe).CombinedOutput()
	if err != nil {
		t.Fatalf("probe exited with error: %v\noutput:\n%s", err, out)
	}
	if !strings.Contains(string(out), "alive") {
		t.Fatalf("probe didn't print heartbeat; output:\n%s", out)
	}
}

// TestProbe_SurvivesWhenLaunchedHidden confirms a hidden console does not
// terminate the process while the foreground-window policy is disabled.
func TestProbe_SurvivesWhenLaunchedHidden(t *testing.T) {
	probe := buildProbe(t)

	cmd := exec.Command(probe)
	cmd.SysProcAttr = &syscall.SysProcAttr{
		HideWindow:    true,
		CreationFlags: 0x00000010, // CREATE_NEW_CONSOLE
	}

	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("probe exited with error: %v\noutput:\n%s", err, out)
	}
	if !strings.Contains(string(out), "alive") {
		t.Fatalf("probe didn't print heartbeat; output:\n%s", out)
	}
}

func buildProbe(t *testing.T) string {
	t.Helper()
	out := filepath.Join(t.TempDir(), "probe.exe")
	cmd := exec.Command("go", "build", "-o", out, "./testdata/probe")
	if combined, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("build probe: %v\n%s", err, combined)
	}
	return out
}

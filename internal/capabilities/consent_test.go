// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestRiskConsentGatePromptsOnceForConcurrentRequests(t *testing.T) {
	var calls atomic.Int32
	gate := newRiskConsentGate(func(context.Context, riskRequest) (bool, error) {
		calls.Add(1)
		time.Sleep(25 * time.Millisecond)
		return true, nil
	})

	var wg sync.WaitGroup
	errs := make(chan error, 8)
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			errs <- gate.Require(context.Background(), riskRequest{
				Kind:    "ps.exec",
				Summary: "test",
			})
		}()
	}
	wg.Wait()
	close(errs)

	for err := range errs {
		if err != nil {
			t.Fatalf("Require returned error: %v", err)
		}
	}
	if got := calls.Load(); got != 1 {
		t.Fatalf("prompt calls = %d, want 1", got)
	}
}

func TestRiskConsentGateCachesDenial(t *testing.T) {
	var calls atomic.Int32
	gate := newRiskConsentGate(func(context.Context, riskRequest) (bool, error) {
		calls.Add(1)
		return false, nil
	})

	for i := 0; i < 2; i++ {
		err := gate.Require(context.Background(), riskRequest{Kind: "fs.delete"})
		if !errors.Is(err, errRiskDenied) {
			t.Fatalf("Require error = %v, want errRiskDenied", err)
		}
	}
	if got := calls.Load(); got != 1 {
		t.Fatalf("prompt calls = %d, want 1", got)
	}
}

func TestRiskPromptTextWarnsAboutRemainderOfSession(t *testing.T) {
	text := riskPromptText(riskRequest{
		Kind:    "ps.exec",
		Summary: "Runs arbitrary code.",
	})
	for _, want := range []string{
		"ps.exec",
		"Runs arbitrary code.",
		"remainder of this session",
		"Only choose Yes if you trust that person",
	} {
		if !strings.Contains(text, want) {
			t.Fatalf("riskPromptText missing %q in %q", want, text)
		}
	}
}

func TestFsDeleteRequiresConsentBeforeRemoving(t *testing.T) {
	path := filepath.Join(t.TempDir(), "victim.txt")
	if err := os.WriteFile(path, []byte("keep"), 0o644); err != nil {
		t.Fatal(err)
	}

	var calls atomic.Int32
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) {
		calls.Add(1)
		return false, nil
	})

	_, err := fsDelete(context.Background(), rawArgs(t, map[string]interface{}{"path": path}))
	if !errors.Is(err, errRiskDenied) {
		t.Fatalf("fsDelete error = %v, want errRiskDenied", err)
	}
	if _, statErr := os.Stat(path); statErr != nil {
		t.Fatalf("file was removed despite denied consent: %v", statErr)
	}
	if got := calls.Load(); got != 1 {
		t.Fatalf("prompt calls = %d, want 1", got)
	}
}

func TestFsDeleteRemovesFileAfterConsent(t *testing.T) {
	path := filepath.Join(t.TempDir(), "victim.txt")
	if err := os.WriteFile(path, []byte("delete"), 0o644); err != nil {
		t.Fatal(err)
	}

	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) {
		return true, nil
	})

	res, err := fsDelete(context.Background(), rawArgs(t, map[string]interface{}{"path": path}))
	if err != nil {
		t.Fatalf("fsDelete returned error: %v", err)
	}
	if _, statErr := os.Stat(path); !errors.Is(statErr, os.ErrNotExist) {
		t.Fatalf("file still exists after delete, stat error = %v", statErr)
	}
	out, ok := res.(map[string]interface{})
	if !ok || out["deleted"] != true {
		t.Fatalf("fsDelete result = %#v, want deleted=true", res)
	}
}

func TestGuardDeletePathRejectsExactProtectedDirectories(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("Windows path guard")
	}
	for _, path := range []string{
		`C:\Windows\System32`,
		`C:\Windows\System32\drivers\etc\hosts`,
		`C:\Program Files`,
		`C:\Program Files\App`,
	} {
		if err := guardDeletePath(path); err == nil {
			t.Fatalf("guardDeletePath(%q) returned nil, want error", path)
		}
	}
}

func TestProcKillRefusesCurrentProcessBeforeConsent(t *testing.T) {
	var calls atomic.Int32
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) {
		calls.Add(1)
		return true, nil
	})

	_, err := procKill(context.Background(), rawArgs(t, map[string]interface{}{"pid": os.Getpid()}))
	if err == nil || !strings.Contains(err.Error(), "current handoff process") {
		t.Fatalf("procKill error = %v, want current-process refusal", err)
	}
	if got := calls.Load(); got != 0 {
		t.Fatalf("prompt calls = %d, want 0", got)
	}
}

func TestPsExecUsesSessionConsentInsteadOfEnvironmentGate(t *testing.T) {
	if _, err := exec.LookPath("powershell.exe"); err != nil {
		t.Skip("powershell.exe not available")
	}
	t.Setenv("HANDOFF_ALLOW_PSEXEC", "")
	psMu.Lock()
	psHistory = nil
	psMu.Unlock()

	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) {
		return true, nil
	})

	res, err := psExec()(context.Background(), rawArgs(t, map[string]interface{}{
		"script": "Write-Output ok",
	}))
	if err != nil {
		t.Fatalf("ps.exec returned error: %v", err)
	}
	out, ok := res.(map[string]interface{})
	if !ok {
		t.Fatalf("ps.exec result = %#v, want map", res)
	}
	if out["ok"] != true {
		t.Fatalf("ps.exec ok = %#v, want true", out["ok"])
	}
	stdout, _ := out["stdout"].(string)
	if !strings.Contains(stdout, "ok") {
		t.Fatalf("ps.exec stdout = %q, want ok", stdout)
	}
}

func withSessionRiskPrompt(t *testing.T, prompt riskPrompt) {
	t.Helper()
	sessionRiskMu.Lock()
	old := sessionRiskConsent
	sessionRiskConsent = newRiskConsentGate(prompt)
	sessionRiskMu.Unlock()
	t.Cleanup(func() {
		sessionRiskMu.Lock()
		sessionRiskConsent = old
		sessionRiskMu.Unlock()
	})
}

func rawArgs(t *testing.T, in map[string]interface{}) map[string]json.RawMessage {
	t.Helper()
	out := make(map[string]json.RawMessage, len(in))
	for k, v := range in {
		b, err := json.Marshal(v)
		if err != nil {
			t.Fatalf("marshal %s: %v", k, err)
		}
		out[k] = b
	}
	return out
}

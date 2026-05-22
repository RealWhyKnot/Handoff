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
	"strconv"
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

func TestPsExecRejectsOversizedScript(t *testing.T) {
	t.Setenv("HANDOFF_ALLOW_PSEXEC", "")
	psMu.Lock()
	psHistory = nil
	psMu.Unlock()

	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) {
		return true, nil
	})

	_, err := psExec()(context.Background(), rawArgs(t, map[string]interface{}{
		"script": strings.Repeat("x", psScriptCap+1),
	}))
	if err == nil {
		t.Fatal("ps.exec did not reject oversized script")
	}
	if !strings.Contains(err.Error(), "cap is 65536") {
		t.Fatalf("ps.exec err = %v, want cap is 65536", err)
	}
}

func TestFsSearchRejectsRelativePath(t *testing.T) {
	_, err := fsSearch(context.Background(), rawArgs(t, map[string]interface{}{
		"path": "relative/path",
		"pattern": "*.txt",
	}))
	if err == nil || !strings.Contains(err.Error(), "absolute") {
		t.Fatalf("fsSearch err = %v, want absolute-path error", err)
	}
}

func TestFsSearchMatchesPatternAndRespectsLimits(t *testing.T) {
	root := t.TempDir()
	for i := 0; i < 120; i++ {
		if err := os.WriteFile(filepath.Join(root, "log-"+strconv.Itoa(i)+".txt"), []byte("x"), 0o644); err != nil {
			t.Fatal(err)
		}
		if i == 0 {
			if err := os.MkdirAll(filepath.Join(root, ".hidden"), 0o755); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(root, ".hidden", "secret.log"), []byte("x"), 0o644); err != nil {
				t.Fatal(err)
			}
		}
	}

	result, err := fsSearch(context.Background(), rawArgs(t, map[string]interface{}{
		"path":        root,
		"pattern":     "*.txt",
		"max_results": 20,
	}))
	if err != nil {
		t.Fatalf("fsSearch returned error: %v", err)
	}
	payload, ok := result.(map[string]interface{})
	if !ok {
		t.Fatalf("fsSearch result = %#v", result)
	}
	count, ok := payload["count"].(int)
	if !ok {
		t.Fatalf("count type = %T, want int", payload["count"])
	}
	if count != 20 {
		t.Fatalf("count = %d, want 20", count)
	}
	entries, ok := payload["entries"].([]map[string]interface{})
	if !ok || len(entries) != 20 {
		t.Fatalf("entries = %#v, want 20", payload["entries"])
	}
	for _, row := range entries {
		name, ok := row["name"].(string)
		if !ok {
			t.Fatalf("entry name type = %T, want string", row["name"])
		}
		if strings.HasPrefix(name, ".") {
			t.Fatalf("hidden file returned despite include_hidden=false: %q", name)
		}
	}

	_, err = fsSearch(context.Background(), rawArgs(t, map[string]interface{}{
		"path": root,
		"pattern": "*.log",
		"include_hidden": true,
	}))
	if err != nil {
		t.Fatalf("fsSearch hidden include error: %v", err)
	}
}

func TestProcFindRejectsOverLongQuery(t *testing.T) {
	_, err := procFind(context.Background(), rawArgs(t, map[string]interface{}{
		"query": strings.Repeat("x", 130),
	}))
	if err == nil || !strings.Contains(err.Error(), "too long") {
		t.Fatalf("procFind err = %v, want query too long", err)
	}
}

func TestNetConnectionsRejectsInvalidState(t *testing.T) {
	_, err := netConnections(context.Background(), rawArgs(t, map[string]interface{}{
		"state": "bad-state",
	}))
	if err == nil || !strings.Contains(err.Error(), "state must") {
		t.Fatalf("netConnections err = %v, want validation error", err)
	}
}

func TestResolveConnectionStateDefaultsToEstablished(t *testing.T) {
	state, err := resolveConnectionState("")
	if err != nil {
		t.Fatalf("resolveConnectionState empty err = %v", err)
	}
	if state != "established" {
		t.Fatalf("resolveConnectionState empty = %q, want established", state)
	}
}

func TestResolveConnectionStateNormalizesCaseAndWhitespace(t *testing.T) {
	state, err := resolveConnectionState("  Listen ")
	if err != nil {
		t.Fatalf("resolveConnectionState Listen err = %v", err)
	}
	if state != "listen" {
		t.Fatalf("resolveConnectionState Listen = %q, want listen", state)
	}
}

func TestRegQueryRejectsBadHive(t *testing.T) {
	_, err := regQuery(context.Background(), rawArgs(t, map[string]interface{}{
		"hive": "HKBOGUS",
		"key":  "SOFTWARE",
	}))
	if err == nil || !strings.Contains(err.Error(), "hive must be") {
		t.Fatalf("regQuery err = %v, want hive validation error", err)
	}
}

func TestRegQueryRejectsMissingKey(t *testing.T) {
	_, err := regQuery(context.Background(), rawArgs(t, map[string]interface{}{
		"hive": "HKLM",
	}))
	if err == nil || !strings.Contains(err.Error(), "'key' is required") {
		t.Fatalf("regQuery err = %v, want key required error", err)
	}
}

func TestRegQueryRejectsPathTraversal(t *testing.T) {
	_, err := regQuery(context.Background(), rawArgs(t, map[string]interface{}{
		"hive": "HKLM",
		"key":  "SOFTWARE\\..\\SECRET",
	}))
	if err == nil || !strings.Contains(err.Error(), "..") {
		t.Fatalf("regQuery err = %v, want traversal rejection", err)
	}
}

func TestRegQueryRejectsBadCharacters(t *testing.T) {
	_, err := regQuery(context.Background(), rawArgs(t, map[string]interface{}{
		"hive": "HKLM",
		"key":  "SOFTWARE;DROP",
	}))
	if err == nil || !strings.Contains(err.Error(), "unsupported characters") {
		t.Fatalf("regQuery err = %v, want character rejection", err)
	}
}

func TestTaskListRejectsInvalidState(t *testing.T) {
	_, err := taskList(context.Background(), rawArgs(t, map[string]interface{}{
		"state": "halfway",
	}))
	if err == nil || !strings.Contains(err.Error(), "state must be") {
		t.Fatalf("taskList err = %v, want state validation", err)
	}
}

func TestSysEnvRejectsInvalidScope(t *testing.T) {
	_, err := sysEnv(context.Background(), rawArgs(t, map[string]interface{}{
		"scope": "session",
	}))
	if err == nil || !strings.Contains(err.Error(), "scope must be") {
		t.Fatalf("sysEnv err = %v, want scope validation", err)
	}
}

func TestEvtProvidersRejectsOverLongPrefix(t *testing.T) {
	_, err := evtProviders(context.Background(), rawArgs(t, map[string]interface{}{
		"name_prefix": strings.Repeat("a", 200),
	}))
	if err == nil || !strings.Contains(err.Error(), "too long") {
		t.Fatalf("evtProviders err = %v, want prefix length validation", err)
	}
}

func TestFsHeadRejectsRelativePath(t *testing.T) {
	_, err := fsHead(context.Background(), rawArgs(t, map[string]interface{}{
		"path": "relative",
	}))
	if err == nil || !strings.Contains(err.Error(), "absolute") {
		t.Fatalf("fsHead err = %v, want absolute-path validation", err)
	}
}

func TestFsTailRejectsRelativePath(t *testing.T) {
	_, err := fsTail(context.Background(), rawArgs(t, map[string]interface{}{
		"path": "relative",
	}))
	if err == nil || !strings.Contains(err.Error(), "absolute") {
		t.Fatalf("fsTail err = %v, want absolute-path validation", err)
	}
}

func TestFsHeadReturnsLineWindow(t *testing.T) {
	dir := t.TempDir()
	target := filepath.Join(dir, "log.txt")
	var sb strings.Builder
	for i := 0; i < 200; i++ {
		sb.WriteString("line-")
		sb.WriteString(strconv.Itoa(i))
		sb.WriteString("\n")
	}
	if err := os.WriteFile(target, []byte(sb.String()), 0o644); err != nil {
		t.Fatal(err)
	}

	res, err := fsHead(context.Background(), rawArgs(t, map[string]interface{}{
		"path":  target,
		"lines": 10,
	}))
	if err != nil {
		t.Fatalf("fsHead err = %v", err)
	}
	payload := res.(map[string]interface{})
	lines := payload["lines"].([]string)
	if len(lines) != 10 || lines[0] != "line-0" || lines[9] != "line-9" {
		t.Fatalf("fsHead lines = %#v", lines)
	}
}

func TestFsTailReturnsTrailingLines(t *testing.T) {
	dir := t.TempDir()
	target := filepath.Join(dir, "log.txt")
	var sb strings.Builder
	for i := 0; i < 200; i++ {
		sb.WriteString("line-")
		sb.WriteString(strconv.Itoa(i))
		sb.WriteString("\n")
	}
	if err := os.WriteFile(target, []byte(sb.String()), 0o644); err != nil {
		t.Fatal(err)
	}

	res, err := fsTail(context.Background(), rawArgs(t, map[string]interface{}{
		"path":  target,
		"lines": 5,
	}))
	if err != nil {
		t.Fatalf("fsTail err = %v", err)
	}
	payload := res.(map[string]interface{})
	lines := payload["lines"].([]string)
	if len(lines) != 5 || lines[0] != "line-195" || lines[4] != "line-199" {
		t.Fatalf("fsTail lines = %#v", lines)
	}
}

func TestFsStatReturnsFileMetadata(t *testing.T) {
	dir := t.TempDir()
	target := filepath.Join(dir, "thing.txt")
	if err := os.WriteFile(target, []byte("hello"), 0o644); err != nil {
		t.Fatal(err)
	}
	res, err := fsStat(context.Background(), rawArgs(t, map[string]interface{}{
		"path": target,
	}))
	if err != nil {
		t.Fatalf("fsStat err = %v", err)
	}
	payload := res.(map[string]interface{})
	if payload["name"].(string) != "thing.txt" {
		t.Fatalf("name = %v", payload["name"])
	}
	if payload["is_dir"].(bool) {
		t.Fatalf("is_dir should be false")
	}
	if payload["size"].(int64) != 5 {
		t.Fatalf("size = %v", payload["size"])
	}
}

func TestFsTreeReturnsEntriesWithCap(t *testing.T) {
	dir := t.TempDir()
	for i := 0; i < 10; i++ {
		sub := filepath.Join(dir, "sub-"+strconv.Itoa(i))
		if err := os.MkdirAll(sub, 0o755); err != nil {
			t.Fatal(err)
		}
		for j := 0; j < 5; j++ {
			if err := os.WriteFile(filepath.Join(sub, "f-"+strconv.Itoa(j)+".txt"), []byte("x"), 0o644); err != nil {
				t.Fatal(err)
			}
		}
	}
	res, err := fsTree(context.Background(), rawArgs(t, map[string]interface{}{
		"path":        dir,
		"max_depth":   2,
		"max_entries": 100,
	}))
	if err != nil {
		t.Fatalf("fsTree err = %v", err)
	}
	payload := res.(map[string]interface{})
	count := payload["count"].(int)
	if count == 0 || count > 100 {
		t.Fatalf("count = %d, want 1..100", count)
	}
}

func TestNetTLSRejectsBadHost(t *testing.T) {
	_, err := netTLS(context.Background(), rawArgs(t, map[string]interface{}{
		"host": "example.com; rm -rf /",
	}))
	if err == nil || !strings.Contains(err.Error(), "invalid characters") {
		t.Fatalf("netTLS err = %v, want host validation", err)
	}
}

func TestNetTLSRejectsBadPort(t *testing.T) {
	_, err := netTLS(context.Background(), rawArgs(t, map[string]interface{}{
		"host": "example.com",
		"port": 70000,
	}))
	if err == nil || !strings.Contains(err.Error(), "port") {
		t.Fatalf("netTLS err = %v, want port validation", err)
	}
}

func TestAppListRejectsOverLongPrefix(t *testing.T) {
	_, err := appList(context.Background(), rawArgs(t, map[string]interface{}{
		"name_prefix": strings.Repeat("a", 200),
	}))
	if err == nil || !strings.Contains(err.Error(), "too long") {
		t.Fatalf("appList err = %v, want prefix validation", err)
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

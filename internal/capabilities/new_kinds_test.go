// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

func allowConsent(t *testing.T) {
	t.Helper()
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return true, nil })
}

func TestFsWriteCreatesAndRefusesToClobber(t *testing.T) {
	allowConsent(t)
	path := filepath.Join(t.TempDir(), "notes.txt")

	res, err := fsWrite(context.Background(), rawArgs(t, map[string]interface{}{
		"path":    path,
		"content": "line one\nline two",
		"newline": "lf",
	}))
	if err != nil {
		t.Fatalf("fsWrite: %v", err)
	}
	out := res.(map[string]interface{})
	if out["created"] != true {
		t.Fatalf("created = %v, want true", out["created"])
	}

	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(data) != "line one\nline two" {
		t.Fatalf("content = %q", string(data))
	}

	// Writing over an existing file needs to be asked for explicitly.
	if _, err := fsWrite(context.Background(), rawArgs(t, map[string]interface{}{
		"path":    path,
		"content": "clobber",
	})); err == nil {
		t.Fatal("expected a refusal without overwrite")
	}
}

func TestFsWriteHonoursNewlineAndAppend(t *testing.T) {
	allowConsent(t)
	path := filepath.Join(t.TempDir(), "crlf.txt")

	if _, err := fsWrite(context.Background(), rawArgs(t, map[string]interface{}{
		"path":    path,
		"content": "a\nb",
		"newline": "crlf",
	})); err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(path)
	if string(data) != "a\r\nb" {
		t.Fatalf("crlf content = %q", string(data))
	}

	if _, err := fsWrite(context.Background(), rawArgs(t, map[string]interface{}{
		"path":    path,
		"content": "\nc",
		"newline": "crlf",
		"append":  true,
	})); err != nil {
		t.Fatal(err)
	}
	data, _ = os.ReadFile(path)
	if string(data) != "a\r\nb\r\nc" {
		t.Fatalf("appended content = %q", string(data))
	}
}

func TestFsWriteRejectsRelativePathAndOversizeContent(t *testing.T) {
	allowConsent(t)
	if _, err := fsWrite(context.Background(), rawArgs(t, map[string]interface{}{
		"path":    "relative.txt",
		"content": "x",
	})); err == nil || !strings.Contains(err.Error(), "absolute") {
		t.Fatalf("err = %v, want an absolute-path error", err)
	}

	if _, err := fsWrite(context.Background(), rawArgs(t, map[string]interface{}{
		"path":    filepath.Join(t.TempDir(), "big.txt"),
		"content": strings.Repeat("x", fsWriteCap+1),
	})); err == nil || !strings.Contains(err.Error(), "cap is") {
		t.Fatalf("err = %v, want a size cap error", err)
	}
}

func TestFsWriteAsksBeforeTouchingTheFilesystem(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "denied.txt")
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return false, nil })

	if _, err := fsWrite(context.Background(), rawArgs(t, map[string]interface{}{
		"path":    path,
		"content": "nope",
	})); err == nil {
		t.Fatal("expected the consent refusal")
	}
	if _, err := os.Stat(path); err == nil {
		t.Fatal("a refused write created the file anyway")
	}
}

func TestNetResolveLocalhost(t *testing.T) {
	res, err := netResolve(context.Background(), rawArgs(t, map[string]interface{}{"name": "localhost"}))
	if err != nil {
		t.Fatalf("netResolve: %v", err)
	}
	out := res.(map[string]interface{})
	if out["resolved"] != true {
		t.Fatalf("localhost did not resolve: %#v", out)
	}
	if len(out["addresses"].([]string)) == 0 {
		t.Fatal("no addresses returned")
	}
}

func TestNetResolveReportsFailureWithoutErroring(t *testing.T) {
	// A name that does not resolve is an answer, not a transport failure, so
	// the command succeeds and the result says what happened.
	res, err := netResolve(context.Background(), rawArgs(t, map[string]interface{}{
		"name": "this-name-should-not-exist.invalid",
	}))
	if err != nil {
		t.Fatalf("netResolve returned a transport error: %v", err)
	}
	out := res.(map[string]interface{})
	if out["resolved"] != false {
		t.Fatalf("expected resolved=false, got %#v", out)
	}
}

func TestNetResolveValidatesName(t *testing.T) {
	for _, bad := range []string{"", strings.Repeat("a", 300), "bad host;name"} {
		if _, err := netResolve(context.Background(), rawArgs(t, map[string]interface{}{"name": bad})); err == nil {
			t.Fatalf("name %q was accepted", bad)
		}
	}
}

func TestScreenshotRetriesSmallerThenGivesUp(t *testing.T) {
	var widths []int
	orig := captureFunc
	t.Cleanup(func() { captureFunc = orig })

	// Always oversized: the handler should try once at half width and then
	// report the size rather than shipping something the relay will reject.
	captureFunc = func(_ context.Context, opts screenshotOptions) (screenshot, error) {
		widths = append(widths, opts.MaxWidth)
		return screenshot{Data: make([]byte, screenshotBudget), Width: opts.MaxWidth, Height: 100}, nil
	}

	_, err := captureWithinBudget(context.Background(), screenshotOptions{MaxWidth: 1600, Format: "jpeg"})
	if err == nil || !strings.Contains(err.Error(), "over the") {
		t.Fatalf("err = %v, want a size-limit error", err)
	}
	if len(widths) != 2 || widths[0] != 1600 || widths[1] != 800 {
		t.Fatalf("capture widths = %v, want one retry at half width", widths)
	}
}

func TestScreenshotReturnsImageWithinBudget(t *testing.T) {
	orig := captureFunc
	t.Cleanup(func() { captureFunc = orig })
	captureFunc = func(_ context.Context, opts screenshotOptions) (screenshot, error) {
		return screenshot{Data: []byte("not-a-real-image"), Width: 1280, Height: 720, Scaled: true}, nil
	}

	res, err := captureWithinBudget(context.Background(), screenshotOptions{MaxWidth: 1600, Format: "jpeg"})
	if err != nil {
		t.Fatal(err)
	}
	out := res.(map[string]interface{})
	if out["width"] != 1280 || out["height"] != 720 || out["scaled"] != true {
		t.Fatalf("dimensions not reported: %#v", out)
	}
	if out["image_base64"] == "" || out["sha256"] == "" {
		t.Fatalf("image payload missing: %#v", out)
	}
}

func TestScreenshotRequiresConsent(t *testing.T) {
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return false, nil })
	orig := captureFunc
	t.Cleanup(func() { captureFunc = orig })
	captured := false
	captureFunc = func(context.Context, screenshotOptions) (screenshot, error) {
		captured = true
		return screenshot{Data: []byte("x")}, nil
	}

	if _, err := screenshotHandler(context.Background(), rawArgs(t, map[string]interface{}{})); err == nil {
		t.Fatal("expected the consent refusal")
	}
	if captured {
		t.Fatal("the screen was captured despite a refusal")
	}
}

func TestNewKindsAreRegisteredAndDiscoverable(t *testing.T) {
	r := specRouter(t)
	for _, kind := range []string{"fs.write", "net.resolve", "svc.status", "sys.screenshot", "control.cancel"} {
		if _, ok := r.SpecFor(kind); !ok {
			t.Fatalf("%s is not registered", kind)
		}
	}
}

func TestReadOnlySessionSurvivesCapabilityRegistration(t *testing.T) {
	// DenyAllRisky ran before RegisterAll, and RegisterAll rebuilt the ledger,
	// so --consent deny silently allowed every risky command.
	t.Cleanup(func() {
		sessionConsentMu.Lock()
		denyAllRisky = false
		sessionConsentMu.Unlock()
		resetRiskConsent()
	})

	DenyAllRisky()
	RegisterAll(dispatch.New(), nil)

	path := filepath.Join(t.TempDir(), "should-not-exist.txt")
	if _, err := fsWrite(context.Background(), rawArgs(t, map[string]interface{}{
		"path":    path,
		"content": "nope",
	})); err == nil {
		t.Fatal("a read-only session wrote a file")
	}
	if _, err := os.Stat(path); err == nil {
		t.Fatal("a read-only session created the file")
	}

	if _, err := screenshotHandler(context.Background(), rawArgs(t, map[string]interface{}{})); err == nil {
		t.Fatal("a read-only session captured the screen")
	}
}

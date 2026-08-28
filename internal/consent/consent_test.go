// SPDX-License-Identifier: GPL-3.0-or-later
package consent

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func allowAll(calls *atomic.Int32) Prompt {
	return func(context.Context, Request) (bool, error) {
		calls.Add(1)
		return true, nil
	}
}

func TestGrantInOneCategoryDoesNotCoverAnother(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(allowAll(&calls))

	if _, err := l.Require(context.Background(), Request{Kind: "pico.reset"}); err != nil {
		t.Fatalf("pico.reset: %v", err)
	}
	// This is the defect the category split exists to fix: approving a device
	// reboot used to hand over PowerShell and file deletion for the session.
	if _, err := l.Require(context.Background(), Request{Kind: "ps.exec"}); err != nil {
		t.Fatalf("ps.exec: %v", err)
	}
	if _, err := l.Require(context.Background(), Request{Kind: "fs.delete"}); err != nil {
		t.Fatalf("fs.delete: %v", err)
	}
	if got := calls.Load(); got != 3 {
		t.Fatalf("prompt calls = %d, want one per category", got)
	}
}

func TestSecondCommandInTheSameCategoryReusesTheGrant(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(allowAll(&calls))

	for _, kind := range []string{"fs.delete", "fs.mkdir", "fs.upload"} {
		decision, err := l.Require(context.Background(), Request{Kind: kind})
		if err != nil {
			t.Fatalf("%s: %v", kind, err)
		}
		_ = decision
	}
	if got := calls.Load(); got != 1 {
		t.Fatalf("prompt calls = %d, want 1 for one category", got)
	}
}

func TestConcurrentRequestsInOneCategoryPromptOnce(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(func(context.Context, Request) (bool, error) {
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
			_, err := l.Require(context.Background(), Request{Kind: "ps.exec"})
			errs <- err
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

func TestConcurrentRequestsInDifferentCategoriesPromptSeparately(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(func(context.Context, Request) (bool, error) {
		calls.Add(1)
		time.Sleep(10 * time.Millisecond)
		return true, nil
	})

	var wg sync.WaitGroup
	for _, kind := range []string{"ps.exec", "fs.delete", "svc.control"} {
		wg.Add(1)
		go func(k string) {
			defer wg.Done()
			if _, err := l.Require(context.Background(), Request{Kind: k}); err != nil {
				t.Errorf("%s: %v", k, err)
			}
		}(kind)
	}
	wg.Wait()

	if got := calls.Load(); got != 3 {
		t.Fatalf("prompt calls = %d, want 3", got)
	}
}

func TestDenialIsNotTerminalForTheSession(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(func(context.Context, Request) (bool, error) {
		calls.Add(1)
		return false, nil
	})
	now := time.Now()
	l.now = func() time.Time { return now }

	if _, err := l.Require(context.Background(), Request{Kind: "fs.delete"}); !errors.Is(err, ErrDenied) {
		t.Fatalf("err = %v, want ErrDenied", err)
	}
	// Immediately re-asking must not reopen the dialog.
	if _, err := l.Require(context.Background(), Request{Kind: "fs.delete"}); !errors.Is(err, ErrDenied) {
		t.Fatalf("err = %v, want ErrDenied", err)
	}
	if got := calls.Load(); got != 1 {
		t.Fatalf("prompt calls = %d, want the denial to be cached", got)
	}

	// After the backoff the host can be asked again. One "no" used to block
	// every risky command for the rest of the session with no way back.
	now = now.Add(DenyBackoff + time.Second)
	if _, err := l.Require(context.Background(), Request{Kind: "fs.delete"}); !errors.Is(err, ErrDenied) {
		t.Fatalf("err = %v, want ErrDenied", err)
	}
	if got := calls.Load(); got != 2 {
		t.Fatalf("prompt calls = %d, want a re-prompt after the backoff", got)
	}
}

func TestExecGrantExpires(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(allowAll(&calls))
	now := time.Now()
	l.now = func() time.Time { return now }

	if _, err := l.Require(context.Background(), Request{Kind: "ps.exec"}); err != nil {
		t.Fatal(err)
	}
	now = now.Add(ExecGrantTTL - time.Minute)
	if _, err := l.Require(context.Background(), Request{Kind: "ps.exec"}); err != nil {
		t.Fatal(err)
	}
	if got := calls.Load(); got != 1 {
		t.Fatalf("prompt calls = %d, want the grant still live", got)
	}

	now = now.Add(2 * time.Minute)
	if _, err := l.Require(context.Background(), Request{Kind: "ps.exec"}); err != nil {
		t.Fatal(err)
	}
	if got := calls.Load(); got != 2 {
		t.Fatalf("prompt calls = %d, want a re-prompt after the TTL", got)
	}
}

func TestPowerIsNeverRemembered(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(allowAll(&calls))

	for i := 0; i < 3; i++ {
		if _, err := l.Require(context.Background(), Request{Kind: "sys.restart"}); err != nil {
			t.Fatal(err)
		}
	}
	if got := calls.Load(); got != 3 {
		t.Fatalf("prompt calls = %d, want a prompt every time", got)
	}
}

func TestRevokeAllForcesAFreshPrompt(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(allowAll(&calls))

	if _, err := l.Require(context.Background(), Request{Kind: "fs.delete"}); err != nil {
		t.Fatal(err)
	}
	if n := l.RevokeAll(); n != 1 {
		t.Fatalf("RevokeAll = %d, want 1", n)
	}
	if len(l.Grants()) != 0 {
		t.Fatalf("grants remain after revoke: %+v", l.Grants())
	}
	if _, err := l.Require(context.Background(), Request{Kind: "fs.delete"}); err != nil {
		t.Fatal(err)
	}
	if got := calls.Load(); got != 2 {
		t.Fatalf("prompt calls = %d, want a re-prompt after revoke", got)
	}
}

func TestDenyEverythingRefusesWithoutPrompting(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(allowAll(&calls))
	l.DenyEverything()

	for _, kind := range []string{"ps.exec", "fs.delete", "tunnel.open"} {
		if _, err := l.Require(context.Background(), Request{Kind: kind}); !errors.Is(err, ErrDenied) {
			t.Fatalf("%s err = %v, want ErrDenied", kind, err)
		}
	}
	if got := calls.Load(); got != 0 {
		t.Fatalf("prompt calls = %d, want a read-only session to never prompt", got)
	}
}

func TestUngatedKindNeedsNoConsent(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(allowAll(&calls))

	decision, err := l.Require(context.Background(), Request{Kind: "sys.info"})
	if err != nil {
		t.Fatal(err)
	}
	if decision != NotRequired {
		t.Fatalf("decision = %q, want %q", decision, NotRequired)
	}
	if got := calls.Load(); got != 0 {
		t.Fatalf("prompt calls = %d, want 0", got)
	}
}

func TestDecisionsAreReportedForTheAuditLog(t *testing.T) {
	var calls atomic.Int32
	l := NewLedger(allowAll(&calls))

	first, err := l.Require(context.Background(), Request{Kind: "fs.delete"})
	if err != nil {
		t.Fatal(err)
	}
	if first != PromptAllow {
		t.Fatalf("first decision = %q, want %q", first, PromptAllow)
	}

	second, err := l.Require(context.Background(), Request{Kind: "fs.mkdir"})
	if err != nil {
		t.Fatal(err)
	}
	if second != GrantCached {
		t.Fatalf("second decision = %q, want %q", second, GrantCached)
	}
}

func TestEveryGatedKindHasACategory(t *testing.T) {
	for kind, category := range kindCategories {
		if category == "" {
			t.Fatalf("%s has an empty category", kind)
		}
		if categoryLabel[category] == "" {
			t.Fatalf("category %q used by %s has no host-facing label", category, kind)
		}
	}
}

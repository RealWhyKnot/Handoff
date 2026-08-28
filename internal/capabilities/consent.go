// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"sync"

	"github.com/RealWhyKnot/Handoff/internal/consent"
)

type riskRequest = consent.Request

type riskPrompt = consent.Prompt

var errRiskDenied = consent.ErrDenied

func riskPromptText(req riskRequest) string { return consent.PromptText(req) }

var (
	sessionConsentMu     sync.Mutex
	sessionConsentLedger = consent.NewLedger(consent.SystemPrompt)
	lastDecisionMu       sync.Mutex
	lastDecision         = map[string]consent.Decision{}
)

// setSessionLedger swaps the session ledger. Tests use it to stub the prompt;
// there is deliberately no environment variable that bypasses consent in a
// shipping build.
func setSessionLedger(l *consent.Ledger) *consent.Ledger {
	sessionConsentMu.Lock()
	defer sessionConsentMu.Unlock()
	old := sessionConsentLedger
	sessionConsentLedger = l
	return old
}

func resetRiskConsent() {
	sessionConsentMu.Lock()
	sessionConsentLedger = consent.NewLedger(consent.SystemPrompt)
	sessionConsentMu.Unlock()
	lastDecisionMu.Lock()
	lastDecision = map[string]consent.Decision{}
	lastDecisionMu.Unlock()
}

// Ledger exposes the session's consent state so the host session loop can show
// and revoke grants without importing this package's internals.
func Ledger() *consent.Ledger {
	sessionConsentMu.Lock()
	defer sessionConsentMu.Unlock()
	return sessionConsentLedger
}

// DenyAllRisky puts the session in read-only mode for the whole run.
func DenyAllRisky() {
	Ledger().DenyEverything()
}

// LastConsentDecision reports what the gate decided for a command id, so the
// audit log can record an actual outcome instead of a fixed string.
func LastConsentDecision(kind string) consent.Decision {
	lastDecisionMu.Lock()
	defer lastDecisionMu.Unlock()
	if d, ok := lastDecision[kind]; ok {
		return d
	}
	return consent.NotRequired
}

func requireRiskConsent(ctx context.Context, kind, summary string) error {
	decision, err := Ledger().Require(ctx, consent.Request{Kind: kind, Summary: summary})
	lastDecisionMu.Lock()
	lastDecision[kind] = decision
	lastDecisionMu.Unlock()
	return err
}

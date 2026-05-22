// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"
)

type riskRequest struct {
	Kind    string
	Summary string
}

type riskPrompt func(context.Context, riskRequest) (bool, error)

type riskConsentGate struct {
	mu      sync.Mutex
	cond    *sync.Cond
	prompt  riskPrompt
	asking  bool
	decided bool
	allowed bool
}

var (
	errRiskDenied = errors.New("host denied risky commands for this session")

	sessionRiskMu      sync.Mutex
	sessionRiskConsent = newRiskConsentGate(promptRiskConsent)
)

func newRiskConsentGate(prompt riskPrompt) *riskConsentGate {
	g := &riskConsentGate{prompt: prompt}
	g.cond = sync.NewCond(&g.mu)
	return g
}

func resetRiskConsent() {
	sessionRiskMu.Lock()
	sessionRiskConsent = newRiskConsentGate(promptRiskConsent)
	sessionRiskMu.Unlock()
}

func requireRiskConsent(ctx context.Context, kind, summary string) error {
	req := riskRequest{Kind: kind, Summary: summary}
	sessionRiskMu.Lock()
	gate := sessionRiskConsent
	sessionRiskMu.Unlock()
	return gate.Require(ctx, req)
}

func (g *riskConsentGate) Require(ctx context.Context, req riskRequest) error {
	for {
		if err := ctx.Err(); err != nil {
			return err
		}

		g.mu.Lock()
		if g.decided {
			allowed := g.allowed
			g.mu.Unlock()
			if allowed {
				return nil
			}
			return errRiskDenied
		}
		if !g.asking {
			g.asking = true
			g.mu.Unlock()

			allowed, err := g.prompt(ctx, req)

			g.mu.Lock()
			if err == nil {
				g.decided = true
				g.allowed = allowed
			}
			g.asking = false
			g.cond.Broadcast()
			g.mu.Unlock()

			if err != nil {
				return err
			}
			if allowed {
				return nil
			}
			return errRiskDenied
		}
		for g.asking && !g.decided {
			g.cond.Wait()
		}
		g.mu.Unlock()
	}
}

func riskPromptText(req riskRequest) string {
	summary := strings.TrimSpace(req.Summary)
	if summary == "" {
		summary = "Run a command that can change this computer."
	}
	return fmt.Sprintf(
		"Handoff is asking to allow risky commands for this session.\n\n"+
			"Requested command: %s\n\n"+
			"%s\n\n"+
			"If you choose Yes, Handoff will allow risky commands from the person using the view URL for the remainder of this session without asking again. Only choose Yes if you trust that person.\n\n"+
			"Choose No to block risky commands for this session.",
		req.Kind,
		summary,
	)
}

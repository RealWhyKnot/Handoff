// SPDX-License-Identifier: GPL-3.0-or-later
//
// Package consent gates the commands that can change the host. Permission is
// granted per category rather than all at once, so approving a device reboot
// does not also hand over PowerShell and file deletion for the rest of the
// session.

package consent

import (
	"context"
	"errors"
	"fmt"
	"sort"
	"strings"
	"sync"
	"time"
)

type Category string

const (
	Files     Category = "files"
	Processes Category = "processes"
	Services  Category = "services"
	Devices   Category = "devices"
	Network   Category = "network"
	Capture   Category = "capture"
	Exec      Category = "exec"
	Power     Category = "power"
)

// Decision is what the audit log records for a command.
type Decision string

const (
	NotRequired  Decision = "not_required"
	PromptAllow  Decision = "prompt_allow"
	PromptDeny   Decision = "prompt_deny"
	GrantCached  Decision = "grant_cached"
	GrantExpired Decision = "grant_expired"
)

var ErrDenied = errors.New("the host declined this request")

// ExecGrantTTL bounds how long a single approval keeps arbitrary script
// execution open. A support call is a burst, not an open-ended licence.
var ExecGrantTTL = 15 * time.Minute

// DenyBackoff stops a declined request from immediately re-opening a dialog,
// without making one "no" permanent for the session.
var DenyBackoff = 60 * time.Second

type Request struct {
	Kind     string
	Category Category
	Summary  string
}

type Prompt func(context.Context, Request) (bool, error)

type Grant struct {
	Category  Category
	GrantedAt time.Time
	ExpiresAt time.Time
}

func (g Grant) Expires() bool { return !g.ExpiresAt.IsZero() }

// categoryLabel is what the host is told they are approving. Keep it plain:
// the reader is the owner of the machine, not an operator.
var categoryLabel = map[Category]string{
	Files:     "read, add, change, and delete your files",
	Processes: "start and stop programs",
	Services:  "start and stop Windows services",
	Devices:   "reprogram attached devices such as a Pico board",
	Network:   "open connections to this PC and to devices on your local network, for example your router",
	Capture:   "capture your screen and set your clipboard",
	Exec:      "run PowerShell commands",
	Power:     "restart or shut down this computer",
}

func categoryOf(kind string) (Category, bool) {
	c, ok := kindCategories[kind]
	return c, ok
}

var kindCategories = map[string]Category{
	"fs.upload":    Files,
	"fs.mkdir":     Files,
	"fs.delete":    Files,
	"fs.write":     Files,
	"pico.save":    Files,
	"proc.kill":    Processes,
	"proc.start":   Processes,
	"svc.control":  Services,
	"pico.bootsel": Devices,
	"pico.flash":   Devices,
	"pico.reset":   Devices,
	"tunnel.open":  Network,
	"net.repair":   Network,
	"sys.screenshot": Capture,
	"clip.set":       Capture,
	"ps.exec":        Exec,
	"sys.restart":    Power,
	"sys.shutdown":   Power,
}

// Ledger holds one session's decisions.
type Ledger struct {
	mu     sync.Mutex
	cond   *sync.Cond
	prompt Prompt

	grants   map[Category]Grant
	denied   map[Category]time.Time
	asking   map[Category]bool
	denyAll  bool
	now      func() time.Time
}

func NewLedger(prompt Prompt) *Ledger {
	l := &Ledger{
		prompt: prompt,
		grants: map[Category]Grant{},
		denied: map[Category]time.Time{},
		asking: map[Category]bool{},
		now:    time.Now,
	}
	l.cond = sync.NewCond(&l.mu)
	return l
}

// DenyEverything runs the session strictly read-only. Every gated command is
// refused without a prompt. There is deliberately no allow-everything twin.
func (l *Ledger) DenyEverything() {
	l.mu.Lock()
	l.denyAll = true
	l.mu.Unlock()
}

func (l *Ledger) Require(ctx context.Context, req Request) (Decision, error) {
	if req.Category == "" {
		if c, ok := categoryOf(req.Kind); ok {
			req.Category = c
		} else {
			return NotRequired, nil
		}
	}

	for {
		if err := ctx.Err(); err != nil {
			return PromptDeny, err
		}

		l.mu.Lock()
		if l.denyAll {
			l.mu.Unlock()
			return PromptDeny, ErrDenied
		}

		now := l.now()
		if g, ok := l.grants[req.Category]; ok {
			if !g.Expires() || now.Before(g.ExpiresAt) {
				l.mu.Unlock()
				return GrantCached, nil
			}
			delete(l.grants, req.Category)
		}

		if until, ok := l.denied[req.Category]; ok {
			if now.Before(until) {
				l.mu.Unlock()
				return PromptDeny, ErrDenied
			}
			delete(l.denied, req.Category)
		}

		if !l.asking[req.Category] {
			l.asking[req.Category] = true
			l.mu.Unlock()

			allowed, err := l.prompt(ctx, req)

			l.mu.Lock()
			delete(l.asking, req.Category)
			if err == nil {
				if allowed {
					l.grants[req.Category] = l.newGrant(req.Category)
				} else {
					l.denied[req.Category] = l.now().Add(DenyBackoff)
				}
			}
			l.cond.Broadcast()
			l.mu.Unlock()

			if err != nil {
				return PromptDeny, err
			}
			if allowed {
				return PromptAllow, nil
			}
			return PromptDeny, ErrDenied
		}

		// Another command in this category is already at the dialog. Wait for
		// that answer instead of stacking a second one on top of it.
		for l.asking[req.Category] {
			l.cond.Wait()
		}
		l.mu.Unlock()
	}
}

func (l *Ledger) newGrant(c Category) Grant {
	g := Grant{Category: c, GrantedAt: l.now()}
	switch c {
	case Exec:
		g.ExpiresAt = g.GrantedAt.Add(ExecGrantTTL)
	case Power:
		// Irreversible and it ends the session that authorised it, so this is
		// never remembered; the grant expires the moment it is used.
		g.ExpiresAt = g.GrantedAt
	}
	return g
}

// Grants lists what is currently approved, newest category name order.
func (l *Ledger) Grants() []Grant {
	l.mu.Lock()
	defer l.mu.Unlock()

	now := l.now()
	out := make([]Grant, 0, len(l.grants))
	for _, g := range l.grants {
		if g.Expires() && !now.Before(g.ExpiresAt) {
			continue
		}
		out = append(out, g)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Category < out[j].Category })
	return out
}

// RevokeAll drops every grant. The next gated command prompts again.
func (l *Ledger) RevokeAll() int {
	l.mu.Lock()
	defer l.mu.Unlock()
	n := len(l.grants)
	l.grants = map[Category]Grant{}
	l.denied = map[Category]time.Time{}
	return n
}

func PromptText(req Request) string {
	summary := strings.TrimSpace(req.Summary)
	if summary == "" {
		summary = "Run a command that can change this computer."
	}
	granted := categoryLabel[req.Category]
	if granted == "" {
		granted = "run this kind of command"
	}

	scope := "for the rest of this session"
	switch req.Category {
	case Exec:
		scope = fmt.Sprintf("for the next %d minutes", int(ExecGrantTTL.Minutes()))
	case Power:
		scope = "this one time"
	}

	return fmt.Sprintf(
		"Your helper is asking for permission to do something on this computer.\n\n"+
			"What they asked for now:\n%s\n\n"+
			"Choosing Yes lets the person with your view link %s:\n  - %s\n\n"+
			"This does not give them anything else. They will have to ask again for a different kind of change.\n\n"+
			"Only choose Yes if you trust this person and asked them for help.",
		summary, scope, granted,
	)
}

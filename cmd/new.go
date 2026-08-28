// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/audit"
	"github.com/RealWhyKnot/Handoff/internal/capabilities"
	"github.com/RealWhyKnot/Handoff/internal/consent"
	"github.com/RealWhyKnot/Handoff/internal/dispatch"
	"github.com/RealWhyKnot/Handoff/internal/relay"
	"github.com/RealWhyKnot/Handoff/internal/stayawake"
	"github.com/RealWhyKnot/Handoff/internal/supportlog"
)

// Version is stamped from main.go.
var Version = "0.1.0"

// New mints a fresh session on the relay, opens the bridge WebSocket,
// and runs the host agent loop until Ctrl+C.
func New(args []string) {
	var consentMode string
	var noKeepAwake bool
	opts, _, err := parseOptions("new", args, func(fs *flag.FlagSet) {
		fs.StringVar(&consentMode, "consent", "ask", "ask or deny; deny runs a strictly read-only session")
		fs.BoolVar(&noKeepAwake, "no-keep-awake", false, "let this computer sleep during the session")
	})
	if err != nil {
		if !errors.Is(err, flag.ErrHelp) {
			fmt.Fprintln(os.Stderr, err)
		}
		os.Exit(2)
	}
	switch consentMode {
	case "ask":
	case "deny":
		capabilities.DenyAllRisky()
	default:
		fmt.Fprintln(os.Stderr, "--consent must be ask or deny")
		os.Exit(2)
	}

	relayBase := opts.Relay
	supportlog.Printf("session start relay=%s version=%s", relayBase, Version)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// Audit log first: the banner reports where it lives, and a host who
	// cannot see what was done has no way to check up on a session.
	log, err := audit.NewInDir(opts.AuditDir)
	if err != nil {
		supportlog.Printf("audit log unavailable: %v", err)
		fmt.Fprintln(os.Stderr, "warning: audit log unavailable:", err)
	}
	defer func() {
		if log != nil {
			_ = log.Close()
		}
	}()

	// Mint a session.
	mintCtx, mintCancel := context.WithTimeout(ctx, 10*time.Second)
	mint, err := relay.Mint(mintCtx, relayBase)
	mintCancel()
	if err != nil {
		supportlog.Printf("mint failed: %v", err)
		fmt.Fprintln(os.Stderr, "could not mint session:", err)
		os.Exit(1)
	}
	sid := shortSid(mint.ViewToken)

	// The view URL is this session's credential; keep it out of a file that
	// outlives the session.
	supportlog.Printf("mint ok sid=%s", sid)

	logPath, _ := supportlog.Path()
	fmt.Println("relay: ", relayBase)
	fmt.Println("log:   ", logPath)
	if log != nil {
		fmt.Println("audit: ", log.Dir())
	}
	if !noKeepAwake && opts.KeepAwake {
		fmt.Println("keeping this PC awake while the session runs (--no-keep-awake to turn off)")
	}
	if consentMode == "deny" {
		fmt.Println("read-only session: every request that would change this PC is refused")
	}
	fmt.Println()
	fmt.Println("session live -- share the view URL with your helper:")
	fmt.Println("  ", mint.ViewURL)
	fmt.Println()
	fmt.Println("press q then Enter to end the session, or ? for other keys.")
	fmt.Println()

	// Handle Ctrl+C.
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
	go func() {
		<-sig
		supportlog.Printf("shutdown requested by signal")
		fmt.Println("\nshutting down...")
		cancel()
	}()

	if !noKeepAwake && opts.KeepAwake {
		stayawake.Start(ctx)
	}
	startKeyboardControl(ctx, cancel, log)

	// Open the bridge WS.
	dialCtx, dialCancel := context.WithTimeout(ctx, 15*time.Second)
	bridge, err := relay.Dial(dialCtx, relayBase, mint.WriteToken)
	dialCancel()
	if err != nil {
		supportlog.Printf("bridge dial failed sid=%s: %v", sid, err)
		fmt.Fprintln(os.Stderr, "could not open bridge:", err)
		os.Exit(1)
	}
	defer bridge.Close()
	supportlog.Printf("bridge connected sid=%s", sid)

	// Dispatcher with all capabilities registered. The bridge is plumbed in so
	// tunnel handlers can push bytes back outside the command_result return path.
	router := dispatch.New()
	capabilities.RegisterAll(router, bridge)
	supportlog.Printf("capabilities registered count=%d", len(router.Kinds()))
	fmt.Printf("ready -- %d capabilities registered\n\n", len(router.Kinds()))

	hostname, _ := os.Hostname()
	if err := bridge.SendHello(ctx, hostname, Version, router.Kinds(), router.Specs()); err != nil {
		supportlog.Printf("hello failed sid=%s: %v", sid, err)
		fmt.Fprintln(os.Stderr, "hello failed:", err)
		os.Exit(1)
	}
	supportlog.Printf("hello sent sid=%s hostname=%s", sid, hostname)
	bridge.StartHeartbeat(ctx)

	jobs := newJobRunner(ctx, sid, router, bridge, log)
	defer jobs.CancelAll()

	// Main loop: receive commands and hand them to cancellable workers.
	backoff := newReconnectBackoff()
	for recvIteration(ctx, sid, bridge, backoff, jobs) {
	}
}

// recvIteration runs one Recv+dispatch step; a panic must not kill the
// process, so it is logged and the loop continues.
func recvIteration(ctx context.Context, sid string, bridge *relay.Bridge, backoff *reconnectBackoff, jobs *jobRunner) (keepGoing bool) {
	defer func() {
		if r := recover(); r != nil {
			supportlog.Printf("recv loop panic sid=%s: %v", sid, r)
			keepGoing = true
		}
	}()
	cmd, err := bridge.Recv(ctx)
	if err != nil {
		if errors.Is(err, context.Canceled) || ctx.Err() != nil {
			supportlog.Printf("recv ended sid=%s err=%v", sid, err)
			return false
		}
		supportlog.Printf("recv ended sid=%s err=%v; reconnecting", sid, err)
		fmt.Fprintln(os.Stderr, "connection lost; reconnecting...")
		return reconnectBridge(ctx, sid, bridge, backoff)
	}
	backoff.Reset()
	supportlog.Printf("command received sid=%s id=%s kind=%s", sid, cmd.ID, cmd.Kind)
	if cmd.Kind == "control.cancel" {
		targetID := readStringExtra(cmd.Extras, "target_id")
		if targetID == "" {
			supportlog.Printf("cancel control missing target sid=%s id=%s", sid, cmd.ID)
			return true
		}
		if jobs.Cancel(targetID) {
			fmt.Printf("[cancel] %s\n", targetID)
			supportlog.Printf("command cancel requested sid=%s id=%s", sid, targetID)
		} else {
			supportlog.Printf("cancel target not active sid=%s id=%s", sid, targetID)
		}
		return true
	}
	if capabilities.IsTunnelFrameKind(cmd.Kind) {
		// Ordered, off the goroutine-per-command path, so stream bytes stay in wire order.
		capabilities.EnqueueTunnelFrame(cmd.Kind, cmd.Extras)
		return true
	}
	// The owner of the machine should be able to see what is being done to it
	// without opening the audit file.
	if summary := summarizeArgs(cmd.Extras); summary != "" {
		fmt.Printf("[cmd] %s  %s  %s\n", cmd.ID, cmd.Kind, summary)
	} else {
		fmt.Printf("[cmd] %s  %s\n", cmd.ID, cmd.Kind)
	}
	jobs.Start(cmd)
	return true
}

type reconnectBackoff struct {
	next time.Duration
}

func newReconnectBackoff() *reconnectBackoff {
	return &reconnectBackoff{next: 500 * time.Millisecond}
}

func (b *reconnectBackoff) Next() time.Duration {
	if b.next <= 0 {
		b.next = 500 * time.Millisecond
	}
	d := b.next
	b.next *= 2
	if b.next > 5*time.Second {
		b.next = 5 * time.Second
	}
	return d
}

func (b *reconnectBackoff) Reset() {
	b.next = 500 * time.Millisecond
}

func reconnectBridge(ctx context.Context, sid string, bridge *relay.Bridge, backoff *reconnectBackoff) bool {
	for {
		delay := backoff.Next()
		if !sleepContext(ctx, delay) {
			supportlog.Printf("reconnect cancelled sid=%s", sid)
			return false
		}

		reconnectCtx, cancel := context.WithTimeout(ctx, 15*time.Second)
		err := bridge.Reconnect(reconnectCtx)
		cancel()
		if err == nil {
			backoff.Reset()
			supportlog.Printf("bridge reconnected sid=%s", sid)
			fmt.Println("reconnected.")
			return true
		}
		if ctx.Err() != nil {
			supportlog.Printf("reconnect ended sid=%s err=%v", sid, ctx.Err())
			return false
		}
		supportlog.Printf("reconnect failed sid=%s delay=%s err=%v", sid, delay, err)
		fmt.Fprintln(os.Stderr, "reconnect failed:", err)
	}
}

func sleepContext(ctx context.Context, d time.Duration) bool {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-timer.C:
		return true
	case <-ctx.Done():
		return false
	}
}

type jobRunner struct {
	rootCtx context.Context
	sid     string
	router  *dispatch.Router
	bridge  *relay.Bridge
	audit   *audit.Logger

	mu     sync.Mutex
	active map[string]context.CancelFunc

	auditFailOnce sync.Once
}

func newJobRunner(rootCtx context.Context, sid string, router *dispatch.Router, bridge *relay.Bridge, auditLog *audit.Logger) *jobRunner {
	return &jobRunner{
		rootCtx: rootCtx,
		sid:     sid,
		router:  router,
		bridge:  bridge,
		audit:   auditLog,
		active:  map[string]context.CancelFunc{},
	}
}

func (r *jobRunner) Start(cmd *relay.Command) {
	var jobCtx context.Context
	var cancel context.CancelFunc
	if cmd.TimeoutMS > 0 {
		jobCtx, cancel = context.WithTimeout(r.rootCtx, time.Duration(cmd.TimeoutMS)*time.Millisecond)
	} else {
		jobCtx, cancel = context.WithCancel(r.rootCtx)
	}

	r.mu.Lock()
	r.active[cmd.ID] = cancel
	r.mu.Unlock()

	go func() {
		defer func() {
			if p := recover(); p != nil {
				supportlog.Printf("job panic sid=%s id=%s: %v", r.sid, cmd.ID, p)
			}
			r.mu.Lock()
			delete(r.active, cmd.ID)
			r.mu.Unlock()
			cancel()
		}()

		out := r.router.Dispatch(jobCtx, cmd.Kind, cmd.Extras)
		if err := jobCtx.Err(); err != nil {
			out.OK = false
			switch {
			case errors.Is(err, context.DeadlineExceeded):
				out.Error = "command timed out"
			case errors.Is(err, context.Canceled):
				out.Error = "command cancelled"
			default:
				out.Error = err.Error()
			}
		}

		r.writeAudit(cmd, out)
		if err := r.bridge.SendCommandResult(r.rootCtx, cmd.ID, out); err != nil {
			supportlog.Printf("send result failed sid=%s id=%s: %v", r.sid, cmd.ID, err)
			fmt.Fprintln(os.Stderr, "could not send result:", err)
			return
		}
		supportlog.Printf("command result sent sid=%s id=%s ok=%v elapsed_ms=%d", r.sid, cmd.ID, out.OK, out.ElapsedMs)
		status := "ok"
		if !out.OK {
			status = "failed"
		}
		if detail := summarizeResult(out); detail != "" {
			fmt.Printf("       -> %s  %dms  %s\n", status, out.ElapsedMs, detail)
		} else {
			fmt.Printf("       -> %s  %dms\n", status, out.ElapsedMs)
		}
	}()
}

func (r *jobRunner) Cancel(id string) bool {
	r.mu.Lock()
	cancel := r.active[id]
	r.mu.Unlock()
	if cancel == nil {
		return false
	}
	cancel()
	return true
}

func (r *jobRunner) CancelAll() {
	r.mu.Lock()
	cancels := make([]context.CancelFunc, 0, len(r.active))
	for _, cancel := range r.active {
		cancels = append(cancels, cancel)
	}
	r.mu.Unlock()
	for _, cancel := range cancels {
		cancel()
	}
}

func (r *jobRunner) writeAudit(cmd *relay.Command, out dispatch.Outcome) {
	if r.audit == nil {
		return
	}
	res := "ok"
	if !out.OK {
		res = "err"
	}
	decision := capabilities.LastConsentDecision(cmd.Kind)
	if decision == consent.PromptDeny {
		res = "denied"
	}
	err := r.audit.Write(audit.Entry{
		SessionID:    r.sid,
		Capability:   cmd.Kind,
		Args:         audit.TrimArgs(cmd.Extras),
		Consent:      string(decision),
		ConsentScope: string(consent.CategoryFor(cmd.Kind)),
		Result:       res,
		ElapsedMs:    out.ElapsedMs,
		Detail:       out.Error,
	})
	if err != nil {
		// A silently broken audit log is worse than a noisy one, but one line
		// per command would be its own problem.
		r.auditFailOnce.Do(func() {
			supportlog.Printf("audit write failed sid=%s: %v", r.sid, err)
			fmt.Fprintln(os.Stderr, "warning: audit log is not recording:", err)
		})
	}
}

func readStringExtra(extras map[string]json.RawMessage, name string) string {
	if extras == nil {
		return ""
	}
	var value string
	if raw, ok := extras[name]; ok {
		_ = json.Unmarshal(raw, &value)
	}
	return value
}

func defaultRelay() string {
	if r := os.Getenv("HANDOFF_RELAY"); r != "" {
		return r
	}
	return "https://handoff.whyknot.dev"
}

func shortSid(t string) string {
	if len(t) > 11 {
		return t[:11] // "n1_AbCdEfGh"
	}
	return t
}

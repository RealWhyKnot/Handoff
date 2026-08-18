// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/audit"
	"github.com/RealWhyKnot/Handoff/internal/capabilities"
	"github.com/RealWhyKnot/Handoff/internal/dispatch"
	"github.com/RealWhyKnot/Handoff/internal/relay"
	"github.com/RealWhyKnot/Handoff/internal/supportlog"
	"github.com/RealWhyKnot/Handoff/internal/visibility"
)

// Version is stamped from main.go.
var Version = "0.1.0"

// New mints a fresh session on the relay, opens the bridge WebSocket,
// and runs the host agent loop until Ctrl+C.
func New(args []string) {
	relayBase := defaultRelay()
	supportlog.Printf("session start relay=%s version=%s", relayBase, Version)
	fmt.Println("relay:", relayBase)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

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
	supportlog.Printf("mint ok sid=%s view_url=%s", sid, mint.ViewURL)
	fmt.Println()
	fmt.Println("session live -- share the view URL with your helper:")
	fmt.Println("  ", mint.ViewURL)
	fmt.Println()
	fmt.Println("press Ctrl+C to end the session at any time.")
	fmt.Println()

	// Audit log.
	log, err := audit.New()
	if err != nil {
		supportlog.Printf("audit log unavailable: %v", err)
		fmt.Fprintln(os.Stderr, "warning: audit log unavailable:", err)
	}
	defer func() {
		if log != nil {
			_ = log.Close()
		}
	}()

	// Handle Ctrl+C.
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
	go func() {
		<-sig
		supportlog.Printf("shutdown requested by signal")
		fmt.Println("\nshutting down...")
		cancel()
	}()

	// Lifecycle hook for foreground-window policy. Currently a no-op.
	visibility.StartWatcher(ctx)

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
	if err := bridge.SendHello(ctx, hostname, Version, router.Kinds()); err != nil {
		supportlog.Printf("hello failed sid=%s: %v", sid, err)
		fmt.Fprintln(os.Stderr, "hello failed:", err)
		os.Exit(1)
	}
	supportlog.Printf("hello sent sid=%s hostname=%s", sid, hostname)

	jobs := newJobRunner(ctx, sid, router, bridge, log)
	defer jobs.CancelAll()

	// Main loop: receive commands and hand them to cancellable workers.
	backoff := newReconnectBackoff()
	for {
		cmd, err := bridge.Recv(ctx)
		if err != nil {
			if errors.Is(err, context.Canceled) || ctx.Err() != nil {
				supportlog.Printf("recv ended sid=%s err=%v", sid, err)
				return
			}
			supportlog.Printf("recv ended sid=%s err=%v; reconnecting", sid, err)
			fmt.Fprintln(os.Stderr, "connection lost; reconnecting...")
			if !reconnectBridge(ctx, sid, bridge, backoff) {
				return
			}
			continue
		}
		backoff.Reset()
		supportlog.Printf("command received sid=%s id=%s kind=%s", sid, cmd.ID, cmd.Kind)
		if cmd.Kind == "control.cancel" {
			targetID := readStringExtra(cmd.Extras, "target_id")
			if targetID == "" {
				supportlog.Printf("cancel control missing target sid=%s id=%s", sid, cmd.ID)
				continue
			}
			if jobs.Cancel(targetID) {
				fmt.Printf("[cancel] %s\n", targetID)
				supportlog.Printf("command cancel requested sid=%s id=%s", sid, targetID)
			} else {
				supportlog.Printf("cancel target not active sid=%s id=%s", sid, targetID)
			}
			continue
		}
		if capabilities.IsTunnelFrameKind(cmd.Kind) {
			// Ordered, off the goroutine-per-command path, so stream bytes stay in wire order.
			capabilities.EnqueueTunnelFrame(cmd.Kind, cmd.Extras)
			continue
		}
		fmt.Printf("[cmd] %s  kind=%s\n", cmd.ID, cmd.Kind)
		jobs.Start(cmd)
	}
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
		if err := r.bridge.SendCommandResult(r.rootCtx, cmd.ID, out.OK, out.Result, out.Error, out.ElapsedMs); err != nil {
			supportlog.Printf("send result failed sid=%s id=%s: %v", r.sid, cmd.ID, err)
			fmt.Fprintln(os.Stderr, "could not send result:", err)
			return
		}
		supportlog.Printf("command result sent sid=%s id=%s ok=%v elapsed_ms=%d", r.sid, cmd.ID, out.OK, out.ElapsedMs)
		fmt.Printf("       -> ok=%v elapsed=%dms\n", out.OK, out.ElapsedMs)
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
	_ = r.audit.Write(audit.Entry{
		SessionID:  r.sid,
		Capability: cmd.Kind,
		Args:       cmd.Extras,
		Consent:    "session",
		Result:     res,
		ElapsedMs:  out.ElapsedMs,
		Detail:     out.Error,
	})
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

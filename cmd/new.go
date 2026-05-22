// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/audit"
	"github.com/RealWhyKnot/Handoff/internal/capabilities"
	"github.com/RealWhyKnot/Handoff/internal/dispatch"
	"github.com/RealWhyKnot/Handoff/internal/relay"
)

// Version is stamped from main.go.
var Version = "0.1.0"

// New mints a fresh session on the relay, opens the bridge WebSocket,
// and runs the host agent loop until Ctrl+C or relay disconnect.
func New(args []string) {
	relayBase := defaultRelay()
	fmt.Println("relay:", relayBase)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// Mint a session.
	mintCtx, mintCancel := context.WithTimeout(ctx, 10*time.Second)
	mint, err := relay.Mint(mintCtx, relayBase)
	mintCancel()
	if err != nil {
		fmt.Fprintln(os.Stderr, "could not mint session:", err)
		os.Exit(1)
	}
	sid := shortSid(mint.ViewToken)
	fmt.Println()
	fmt.Println("session live -- share the view URL with your helper:")
	fmt.Println("  ", mint.ViewURL)
	fmt.Println()
	fmt.Println("press Ctrl+C to end the session at any time.")
	fmt.Println()

	// Audit log.
	log, err := audit.New()
	if err != nil {
		fmt.Fprintln(os.Stderr, "warning: audit log unavailable:", err)
	}
	defer func() {
		if log != nil {
			_ = log.Close()
		}
	}()

	// Dispatcher with all capabilities registered.
	router := dispatch.New()
	capabilities.RegisterAll(router)
	fmt.Printf("ready -- %d capabilities registered\n\n", len(router.Kinds()))

	// Handle Ctrl+C.
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
	go func() {
		<-sig
		fmt.Println("\nshutting down...")
		cancel()
	}()

	// Open the bridge WS.
	dialCtx, dialCancel := context.WithTimeout(ctx, 15*time.Second)
	bridge, err := relay.Dial(dialCtx, relayBase, mint.WriteToken)
	dialCancel()
	if err != nil {
		fmt.Fprintln(os.Stderr, "could not open bridge:", err)
		os.Exit(1)
	}
	defer bridge.Close()

	hostname, _ := os.Hostname()
	if err := bridge.SendHello(ctx, hostname, Version); err != nil {
		fmt.Fprintln(os.Stderr, "hello failed:", err)
		os.Exit(1)
	}

	// Main loop: receive commands, dispatch, send results.
	for {
		cmd, err := bridge.Recv(ctx)
		if err != nil {
			if errors.Is(err, context.Canceled) || errors.Is(err, io.EOF) {
				return
			}
			fmt.Fprintln(os.Stderr, "recv error:", err)
			return
		}
		fmt.Printf("[cmd] %s  kind=%s\n", cmd.ID, cmd.Kind)

		out := router.Dispatch(ctx, cmd.Kind, cmd.Extras)

		// Audit.
		if log != nil {
			res := "ok"
			if !out.OK {
				res = "err"
			}
			_ = log.Write(audit.Entry{
				SessionID:  sid,
				Capability: cmd.Kind,
				Args:       cmd.Extras,
				Consent:    "session",
				Result:     res,
				ElapsedMs:  out.ElapsedMs,
				Detail:     out.Error,
			})
		}

		// Result back to the relay.
		if err := bridge.SendCommandResult(ctx, cmd.ID, out.OK, out.Result, out.Error, out.ElapsedMs); err != nil {
			fmt.Fprintln(os.Stderr, "could not send result:", err)
			return
		}
		fmt.Printf("       -> ok=%v elapsed=%dms\n", out.OK, out.ElapsedMs)
	}
}

func defaultRelay() string {
	if r := os.Getenv("HANDOFF_RELAY"); r != "" {
		return r
	}
	return "https://couchlink.whyknot.dev"
}

func shortSid(t string) string {
	if len(t) > 11 {
		return t[:11] // "n1_AbCdEfGh"
	}
	return t
}

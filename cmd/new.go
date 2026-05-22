// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"
)

// New mints a fresh session on the relay and starts the host agent loop.
// The view URL is printed to stdout for sharing; the loop runs until the
// user presses Ctrl+C or the relay disconnects permanently.
func New(args []string) {
	relay := defaultRelay()

	fmt.Println("relay:", relay)
	fmt.Println("(scaffolding -- full session loop lands in a follow-up commit)")

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
	go func() {
		<-sig
		fmt.Println("\nshutting down...")
		cancel()
	}()

	<-ctx.Done()
}

func defaultRelay() string {
	if r := os.Getenv("HANDOFF_RELAY"); r != "" {
		return r
	}
	return "https://couchlink.whyknot.dev"
}

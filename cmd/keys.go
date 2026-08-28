// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/audit"
	"github.com/RealWhyKnot/Handoff/internal/capabilities"
)

// startKeyboardControl gives the host a way to see and withdraw what they have
// approved without killing the session. It reads stdin only on a terminal, so
// a piped or redirected run is unaffected.
func startKeyboardControl(ctx context.Context, cancel context.CancelFunc, log *audit.Logger) {
	if !stdinIsTerminal() {
		return
	}

	go func() {
		reader := bufio.NewReader(os.Stdin)
		for {
			line, err := reader.ReadString('\n')
			if err != nil {
				return
			}
			select {
			case <-ctx.Done():
				return
			default:
			}

			switch strings.ToLower(strings.TrimSpace(line)) {
			case "q", "quit", "exit":
				fmt.Println("shutting down...")
				cancel()
				return
			case "p":
				printGrants()
			case "r":
				n := capabilities.Ledger().RevokeAll()
				fmt.Printf("revoked %d permission(s); the next request will ask again\n", n)
			case "a":
				if log != nil {
					fmt.Println("audit log:", log.Dir())
				} else {
					fmt.Println("audit log unavailable for this session")
				}
			case "?", "h", "help":
				printKeyHelp()
			case "":
			default:
				printKeyHelp()
			}
		}
	}()
}

func printGrants() {
	grants := capabilities.Ledger().Grants()
	if len(grants) == 0 {
		fmt.Println("nothing is approved right now")
		return
	}
	fmt.Println("approved right now:")
	for _, g := range grants {
		if g.Expires() {
			remaining := time.Until(g.ExpiresAt).Round(time.Minute)
			fmt.Printf("  %-10s expires in %s\n", g.Category, remaining)
			continue
		}
		fmt.Printf("  %-10s for this session\n", g.Category)
	}
}

func printKeyHelp() {
	fmt.Println("keys: q quit  p show what is approved  r revoke approvals  a audit log location  ? this list")
}

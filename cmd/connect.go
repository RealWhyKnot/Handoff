// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
)

// Connect opens an operator viewer in the default browser. v0.1 is intentionally
// thin -- it just hands the URL to the OS. A richer operator CLI grows in v0.2.
func Connect(args []string) {
	if len(args) < 1 {
		fmt.Fprintln(os.Stderr, "usage: handoff connect <view-url-or-token>")
		os.Exit(2)
	}
	arg := args[0]
	url := arg
	if !strings.HasPrefix(arg, "http://") && !strings.HasPrefix(arg, "https://") {
		// Bare token: build the URL against the configured relay.
		relay := strings.TrimRight(defaultRelay(), "/")
		url = relay + "/v/" + arg
	}

	if err := openBrowser(url); err != nil {
		fmt.Fprintf(os.Stderr, "could not open browser: %v\n", err)
		fmt.Fprintln(os.Stderr, "open this URL manually:", url)
		os.Exit(1)
	}
	fmt.Println("opened:", url)
}

func openBrowser(url string) error {
	// rundll32 url.dll,FileProtocolHandler is the most reliable browser opener
	// across Windows variants; cmd /c start has quoting pitfalls with URLs that
	// contain '&'.
	return exec.Command("rundll32", "url.dll,FileProtocolHandler", url).Start()
}

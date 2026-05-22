// Handoff -- token-gated remote debug helper for Windows.
// SPDX-License-Identifier: GPL-3.0-or-later
package main

import (
	"fmt"
	"os"

	"github.com/RealWhyKnot/Handoff/cmd"
)

const version = "0.1.0"

func main() {
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}

	switch os.Args[1] {
	case "new":
		cmd.New(os.Args[2:])
	case "connect":
		cmd.Connect(os.Args[2:])
	case "version", "-v", "--version":
		fmt.Println("handoff", version)
	case "help", "-h", "--help":
		usage()
	default:
		fmt.Fprintf(os.Stderr, "unknown subcommand: %s\n\n", os.Args[1])
		usage()
		os.Exit(2)
	}
}

func usage() {
	fmt.Fprintln(os.Stderr, "handoff -- token-gated remote debug helper for Windows")
	fmt.Fprintln(os.Stderr, "")
	fmt.Fprintln(os.Stderr, "Subcommands:")
	fmt.Fprintln(os.Stderr, "  new                     start a host session, print the view URL")
	fmt.Fprintln(os.Stderr, "  connect <url-or-token>  open an operator viewer (browser)")
	fmt.Fprintln(os.Stderr, "  version                 print version")
	fmt.Fprintln(os.Stderr, "")
	fmt.Fprintln(os.Stderr, "Env:")
	fmt.Fprintln(os.Stderr, "  HANDOFF_RELAY  override the default relay URL (https://couchlink.whyknot.dev)")
}

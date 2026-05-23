// Handoff -- token-gated remote debug helper for Windows.
// SPDX-License-Identifier: GPL-3.0-or-later
package main

import (
	"fmt"
	"os"

	"github.com/RealWhyKnot/Handoff/cmd"
	"github.com/RealWhyKnot/Handoff/internal/supportlog"
)

// version is stamped by the release workflow via
//
//	-ldflags "-X main.version=<tag without v-prefix>"
//
// per the WhyKnot family YYYY.M.D.N scheme. The "dev" default
// covers local builds without ldflags.
var version = "dev"

func main() {
	cmd.Version = version

	if logPath, err := supportlog.Init(); err != nil {
		fmt.Fprintln(os.Stderr, "warning: support log unavailable:", err)
	} else {
		defer supportlog.Close()
		fmt.Println("log:", logPath)
	}

	if code := run(os.Args[1:]); code != 0 {
		os.Exit(code)
	}
}

var newCommand = cmd.New

func run(args []string) int {
	if len(args) < 1 {
		newCommand(nil)
		return 0
	}

	switch args[0] {
	case "new":
		newCommand(args[1:])
	case "connect":
		cmd.Connect(args[1:])
	case "update":
		cmd.Update(args[1:])
	case "version", "-v", "--version":
		fmt.Println("handoff", version)
	case "help", "-h", "--help":
		usage()
	default:
		fmt.Fprintf(os.Stderr, "unknown subcommand: %s\n\n", args[0])
		usage()
		return 2
	}
	return 0
}

func usage() {
	fmt.Fprintln(os.Stderr, "handoff -- token-gated remote debug helper for Windows")
	fmt.Fprintln(os.Stderr, "")
	fmt.Fprintln(os.Stderr, "Running without a subcommand starts a new host session.")
	fmt.Fprintln(os.Stderr, "")
	fmt.Fprintln(os.Stderr, "Subcommands:")
	fmt.Fprintln(os.Stderr, "  new                     start a host session, print the view URL")
	fmt.Fprintln(os.Stderr, "  connect <url-or-token>  open an operator viewer (browser)")
	fmt.Fprintln(os.Stderr, "  update [--check]        fetch a newer handoff.exe from the relay")
	fmt.Fprintln(os.Stderr, "  version                 print version")
	fmt.Fprintln(os.Stderr, "")
	fmt.Fprintln(os.Stderr, "Env:")
	fmt.Fprintln(os.Stderr, "  HANDOFF_RELAY  override the default relay URL (https://handoff.whyknot.dev)")
}

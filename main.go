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

	// Only the long-running subcommands need a support log, and its path is a
	// diagnostic rather than output: printing it on `version` or `--json`
	// corrupts anything parsing stdout.
	if wantsSupportLog(os.Args[1:]) {
		if logPath, err := supportlog.Init(); err != nil {
			fmt.Fprintln(os.Stderr, "warning: support log unavailable:", err)
		} else {
			defer supportlog.Close()
			fmt.Fprintln(os.Stderr, "log:", logPath)
		}
	}

	if code := run(os.Args[1:]); code != 0 {
		os.Exit(code)
	}
}

func wantsSupportLog(args []string) bool {
	sub := ""
	if len(args) > 0 {
		sub = args[0]
	}
	switch sub {
	case "", "new", "tunnel", "menu":
		return true
	}
	return false
}

var (
	newCommand  = cmd.New
	menuCommand = cmd.Menu
)

func run(args []string) int {
	if len(args) < 1 {
		menuCommand(nil)
		return 0
	}

	switch args[0] {
	case "new":
		newCommand(args[1:])
	case "menu":
		menuCommand(args[1:])
	case "connect":
		cmd.Connect(args[1:])
	case "tunnel":
		cmd.Tunnel(args[1:])
	case "exec":
		return cmd.Exec(args[1:])
	case "update":
		cmd.Update(args[1:])
	case "version", "-v", "--version":
		return cmd.PrintVersion(args[1:], version)
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
	fmt.Println("handoff -- token-gated remote debug helper for Windows")
	fmt.Println("")
	fmt.Println("Running without a subcommand shows an interactive menu.")
	fmt.Println("")
	fmt.Println("Subcommands:")
	fmt.Println("  new                        start a host session, print the view URL")
	fmt.Println("  menu                       show the interactive launcher")
	fmt.Println("  connect <url-or-token>     open an operator viewer (browser)")
	fmt.Println("  exec <token> <kind>        run one command against a session, print the result")
	fmt.Println("  tunnel <connect-token>     forward a remote local port to this computer")
	fmt.Println("  update [--check]           fetch a newer handoff.exe from the relay")
	fmt.Println("  version                    print version and file locations")
	fmt.Println("")
	fmt.Println("Common flags:")
	fmt.Println("  --relay URL                relay base URL")
	fmt.Println("  --json                     machine-readable output")
	fmt.Println("  --config FILE              config file path")
	fmt.Println("  --log-dir DIR              where to write the support log")
	fmt.Println("")
	fmt.Println("Env:")
	fmt.Println("  HANDOFF_RELAY      override the default relay URL (https://handoff.whyknot.dev)")
	fmt.Println("  HANDOFF_LOG_DIR    override the support log directory")
	fmt.Println("  HANDOFF_CONFIG     override the config file path")
}

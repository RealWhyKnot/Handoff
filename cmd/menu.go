// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"bufio"
	"fmt"
	"io"
	"os"
	"strconv"
	"strings"
)

// Menu is the interactive launcher shown when handoff.exe is run with no
// subcommand. It lets the user pick between hosting a session and connecting
// as the operator side of a tunnel. When stdin is not a terminal (e.g. piped),
// we keep the prior behavior of falling through to a host session.
func Menu(args []string) {
	if !stdinIsTerminal() {
		New(args)
		return
	}

	reader := bufio.NewReader(os.Stdin)
	for {
		printMenu()
		choice, err := readChoice(reader)
		if err != nil {
			if err == io.EOF {
				return
			}
			fmt.Fprintln(os.Stderr, "could not read input:", err)
			return
		}
		switch choice {
		case "1", "h", "host":
			New(args)
			return
		case "2", "t", "tunnel", "connect":
			runTunnelPrompt(reader, args)
			return
		case "3", "q", "quit", "exit":
			return
		case "":
			// re-prompt
		default:
			fmt.Println("not a valid choice; type 1, 2, or 3.")
		}
	}
}

func printMenu() {
	fmt.Println()
	fmt.Println("What would you like to do?")
	fmt.Println("  1) Host a session (let someone help me with this computer)")
	fmt.Println("  2) Connect to a tunnel (forward a port from someone else's computer)")
	fmt.Println("  3) Quit")
	fmt.Print("Choose [1-3]: ")
}

func readChoice(reader *bufio.Reader) (string, error) {
	line, err := reader.ReadString('\n')
	if err != nil && line == "" {
		return "", err
	}
	return strings.ToLower(strings.TrimSpace(line)), nil
}

func runTunnelPrompt(reader *bufio.Reader, baseArgs []string) {
	fmt.Println()
	fmt.Println("Paste the connect token shown on the session page after the host")
	fmt.Println("approves the tunnel (looks like 'tk_AbCdEfGh').")
	fmt.Print("Connect token: ")
	tokenLine, err := reader.ReadString('\n')
	if err != nil {
		fmt.Fprintln(os.Stderr, "could not read token:", err)
		return
	}
	token := strings.TrimSpace(tokenLine)
	if token == "" {
		fmt.Println("no token entered; aborting.")
		return
	}

	fmt.Print("Local port to bind on this computer [47800]: ")
	portLine, err := reader.ReadString('\n')
	if err != nil {
		fmt.Fprintln(os.Stderr, "could not read port:", err)
		return
	}
	port := 47800
	if v := strings.TrimSpace(portLine); v != "" {
		parsed, perr := strconv.Atoi(v)
		if perr != nil || parsed <= 0 || parsed > 65535 {
			fmt.Println("port must be a number between 1 and 65535.")
			return
		}
		port = parsed
	}

	args := append([]string{token, "--local-port", strconv.Itoa(port)}, baseArgs...)
	Tunnel(args)
}

func stdinIsTerminal() bool {
	fi, err := os.Stdin.Stat()
	if err != nil {
		return false
	}
	return (fi.Mode() & os.ModeCharDevice) != 0
}

// SPDX-License-Identifier: GPL-3.0-or-later
//go:build windows

package cmd

import (
	"bufio"
	"fmt"
	"os"
	"syscall"
	"unsafe"
)

var (
	kernel32                  = syscall.NewLazyDLL("kernel32.dll")
	getConsoleProcessListProc = kernel32.NewProc("GetConsoleProcessList")
)

// ownsConsole reports whether this process is the only one attached to its
// console, which is what a double-clicked exe looks like. In that case the
// window closes the instant main returns, taking any message with it.
func ownsConsole() bool {
	var pids [4]uint32
	n, _, _ := getConsoleProcessListProc.Call(
		uintptr(unsafe.Pointer(&pids[0])),
		uintptr(len(pids)),
	)
	return n == 1
}

func pauseIfOwnConsole() {
	if !ownsConsole() || !stdinIsTerminal() {
		return
	}
	fmt.Print("\npress Enter to close...")
	_, _ = bufio.NewReader(os.Stdin).ReadString('\n')
}

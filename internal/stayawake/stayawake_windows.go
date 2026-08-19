// SPDX-License-Identifier: GPL-3.0-or-later
//go:build windows

package stayawake

import (
	"context"
	"runtime"
	"syscall"
)

const (
	esContinuous     uintptr = 0x80000000
	esSystemRequired uintptr = 0x00000001
)

var (
	kernel32                = syscall.NewLazyDLL("kernel32.dll")
	setThreadExecutionState = kernel32.NewProc("SetThreadExecutionState")
)

// Start keeps the system awake until ctx is done. ES_CONTINUOUS is
// per-thread, so set and clear must run on the same locked OS thread.
func Start(ctx context.Context) {
	go func() {
		runtime.LockOSThread()
		defer runtime.UnlockOSThread()
		setThreadExecutionState.Call(esContinuous | esSystemRequired)
		<-ctx.Done()
		setThreadExecutionState.Call(esContinuous)
	}()
}

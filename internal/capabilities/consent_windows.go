// SPDX-License-Identifier: GPL-3.0-or-later
//go:build windows

package capabilities

import (
	"context"
	"fmt"
	"syscall"
	"unsafe"
)

const (
	messageBoxYesNo         = 0x00000004
	messageBoxIconWarning   = 0x00000030
	messageBoxSetForeground = 0x00010000
	messageBoxTopMost       = 0x00040000
	messageBoxResultYes     = 6
)

var (
	user32         = syscall.NewLazyDLL("user32.dll")
	messageBoxProc = user32.NewProc("MessageBoxW")
)

func promptRiskConsent(ctx context.Context, req riskRequest) (bool, error) {
	if err := ctx.Err(); err != nil {
		return false, err
	}

	title, err := syscall.UTF16PtrFromString("Allow risky Handoff commands?")
	if err != nil {
		return false, err
	}
	text, err := syscall.UTF16PtrFromString(riskPromptText(req))
	if err != nil {
		return false, err
	}

	ret, _, callErr := messageBoxProc.Call(
		0,
		uintptr(unsafe.Pointer(text)),
		uintptr(unsafe.Pointer(title)),
		uintptr(messageBoxYesNo|messageBoxIconWarning|messageBoxSetForeground|messageBoxTopMost),
	)
	if ret == 0 {
		return false, fmt.Errorf("risk consent prompt failed: %w", callErr)
	}
	return ret == messageBoxResultYes, nil
}

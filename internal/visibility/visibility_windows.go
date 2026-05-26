// SPDX-License-Identifier: GPL-3.0-or-later
//go:build windows

package visibility

import (
	"context"
	"errors"
	"fmt"
	"os"
	"sync"
	"syscall"
	"time"
	"unsafe"

	"github.com/RealWhyKnot/Handoff/internal/supportlog"
)

const (
	gwlExStyle               = -20
	wsExToolWindow   uintptr = 0x00000080
	wsExAppWindow    uintptr = 0x00040000
	gwOwner          uintptr = 4
	th32csSnapproc   uintptr = 0x00000002
	dwmwaCloaked     uintptr = 14
	hresultOK        uintptr = 0
	invalidHandle            = ^uintptr(0)
)

var (
	kernel32                 = syscall.NewLazyDLL("kernel32.dll")
	getConsoleWindowProc     = kernel32.NewProc("GetConsoleWindow")
	createToolhelp32Snapshot = kernel32.NewProc("CreateToolhelp32Snapshot")
	process32FirstWProc      = kernel32.NewProc("Process32FirstW")
	process32NextWProc       = kernel32.NewProc("Process32NextW")
	closeHandleProc          = kernel32.NewProc("CloseHandle")

	user32                       = syscall.NewLazyDLL("user32.dll")
	isWindowProc                 = user32.NewProc("IsWindow")
	isWindowVisibleProc          = user32.NewProc("IsWindowVisible")
	getWindowLongPtrWProc        = user32.NewProc("GetWindowLongPtrW")
	getWindowTextLengthWProc     = user32.NewProc("GetWindowTextLengthW")
	getWindowProc                = user32.NewProc("GetWindow")
	enumWindowsProc              = user32.NewProc("EnumWindows")
	getWindowThreadProcessIdProc = user32.NewProc("GetWindowThreadProcessId")

	dwmapi                = syscall.NewLazyDLL("dwmapi.dll")
	dwmGetWindowAttribute = dwmapi.NewProc("DwmGetWindowAttribute")
)

// processEntry32W matches PROCESSENTRY32W on 64-bit Windows. Size must be set
// to unsafe.Sizeof(processEntry32W{}) before the first Process32FirstW call.
type processEntry32W struct {
	Size            uint32
	CntUsage        uint32
	ProcessID       uint32
	DefaultHeapID   uintptr
	ModuleID        uint32
	CntThreads      uint32
	ParentProcessID uint32
	PriClassBase    int32
	Flags           uint32
	ExeFile         [260]uint16
}

// enumCollector is written by enumProc inside EnumWindows. EnumWindows runs
// synchronously on the watcher goroutine and the callback is invoked
// in-thread, so a package-level slice with no lock is safe -- the watcher
// is the only caller.
var enumCollector []uintptr

func enumProc(hwnd uintptr, _ uintptr) uintptr {
	enumCollector = append(enumCollector, hwnd)
	return 1
}

// enumProcCallback is allocated exactly once at package init.
// syscall.NewCallback leaks if re-called per tick.
var enumProcCallback = syscall.NewCallback(enumProc)

var startOnce sync.Once

// StartWatcher launches the visibility watcher goroutine. Idempotent: only
// the first call spawns a worker; subsequent calls are no-ops so a single
// process that runs through multiple commands stays at one watcher.
func StartWatcher(ctx context.Context) {
	startOnce.Do(func() {
		go func() {
			t := time.NewTicker(defaultTick)
			defer t.Stop()
			runWatcher(ctx, t.C, defaultGraceTicks, performCheck, killProcess)
		}()
	})
}

func killProcess(reason string) {
	supportlog.Printf("visibility watcher killing process: %s", reason)
	os.Exit(1)
}

func performCheck() checkResult {
	cw, _, _ := getConsoleWindowProc.Call()
	if cw != 0 && isOwnConsoleVisible(cw) {
		return checkResult{ok: true}
	}

	consoleReason := "console window hidden or cloaked"
	if cw == 0 {
		consoleReason = "no console window (detached or CREATE_NO_WINDOW)"
	}

	parents, err := collectParents()
	if err != nil {
		return checkResult{ok: false, reason: fmt.Sprintf("%s; toolhelp snapshot failed: %v", consoleReason, err)}
	}
	ancestors := buildAncestors(uint32(os.Getpid()), parents)

	enumCollector = enumCollector[:0]
	enumWindowsProc.Call(enumProcCallback, 0)
	for _, hwnd := range enumCollector {
		if isWindowCall(hwnd) == 0 {
			continue
		}
		var pid uint32
		getWindowThreadProcessIdProc.Call(hwnd, uintptr(unsafe.Pointer(&pid)))
		if _, ok := ancestors[pid]; !ok {
			continue
		}
		if isAncestorWindowOnTaskbar(hwnd) {
			return checkResult{ok: true}
		}
	}

	return checkResult{ok: false, reason: consoleReason + "; no visible ancestor window"}
}

func isOwnConsoleVisible(hwnd uintptr) bool {
	if isWindowCall(hwnd) == 0 {
		return false
	}
	if isWindowVisibleCall(hwnd) == 0 {
		return false
	}
	if windowExStyle(hwnd)&wsExToolWindow != 0 {
		return false
	}
	if isCloaked(hwnd) {
		return false
	}
	return true
}

func isAncestorWindowOnTaskbar(hwnd uintptr) bool {
	if isWindowVisibleCall(hwnd) == 0 {
		return false
	}
	ex := windowExStyle(hwnd)
	if ex&wsExToolWindow != 0 {
		return false
	}
	titleLen, _, _ := getWindowTextLengthWProc.Call(hwnd)
	if titleLen == 0 {
		return false
	}
	owner, _, _ := getWindowProc.Call(hwnd, gwOwner)
	if owner != 0 && ex&wsExAppWindow == 0 {
		return false
	}
	if isCloaked(hwnd) {
		return false
	}
	return true
}

func isWindowCall(hwnd uintptr) uintptr {
	r, _, _ := isWindowProc.Call(hwnd)
	return r
}

func isWindowVisibleCall(hwnd uintptr) uintptr {
	r, _, _ := isWindowVisibleProc.Call(hwnd)
	return r
}

func windowExStyle(hwnd uintptr) uintptr {
	// GWL_EXSTYLE is the C constant -20. Go forbids the constant conversion
	// uintptr(int32(-20)) at compile time, so route it through a variable
	// where the int32->uintptr conversion happens at runtime as sign-extension.
	idx := int32(gwlExStyle)
	r, _, _ := getWindowLongPtrWProc.Call(hwnd, uintptr(idx))
	return r
}

func isCloaked(hwnd uintptr) bool {
	var cloaked uint32
	ret, _, _ := dwmGetWindowAttribute.Call(
		hwnd,
		dwmwaCloaked,
		uintptr(unsafe.Pointer(&cloaked)),
		unsafe.Sizeof(cloaked),
	)
	if ret != hresultOK {
		return false
	}
	return cloaked != 0
}

func collectParents() (map[uint32]uint32, error) {
	snap, _, _ := createToolhelp32Snapshot.Call(th32csSnapproc, 0)
	if snap == invalidHandle || snap == 0 {
		return nil, errors.New("CreateToolhelp32Snapshot failed")
	}
	defer closeHandleProc.Call(snap)

	var pe processEntry32W
	pe.Size = uint32(unsafe.Sizeof(pe))

	ret, _, _ := process32FirstWProc.Call(snap, uintptr(unsafe.Pointer(&pe)))
	if ret == 0 {
		return nil, errors.New("Process32FirstW failed")
	}

	parents := map[uint32]uint32{}
	for {
		parents[pe.ProcessID] = pe.ParentProcessID
		next, _, _ := process32NextWProc.Call(snap, uintptr(unsafe.Pointer(&pe)))
		if next == 0 {
			break
		}
	}
	return parents, nil
}

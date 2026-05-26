// SPDX-License-Identifier: GPL-3.0-or-later

// Probe is a test fixture for visibility integration tests. It starts the
// real visibility watcher and prints a heartbeat for a few seconds. If the
// watcher kills the process, os.Exit(1) fires from the watcher goroutine
// and the test sees a non-zero exit. If the watcher stays quiet, the probe
// exits 0 after the deadline.
package main

import (
	"context"
	"fmt"
	"os"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/visibility"
)

func main() {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	visibility.StartWatcher(ctx)

	deadline := time.Now().Add(8 * time.Second)
	for time.Now().Before(deadline) {
		fmt.Println("alive")
		time.Sleep(500 * time.Millisecond)
	}
	os.Exit(0)
}

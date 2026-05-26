// SPDX-License-Identifier: GPL-3.0-or-later

// Package visibility kills the running process when no taskbar-visible
// window can be found in its process ancestry. The check runs once per
// second while a long-running command (host bridge or operator tunnel)
// is active. On non-Windows builds StartWatcher is a no-op.
package visibility

import (
	"context"
	"time"
)

const (
	defaultTick       = time.Second
	defaultGraceTicks = 2
	maxAncestorDepth  = 32
)

type checkResult struct {
	ok     bool
	reason string
}

// runWatcher drives the visibility loop. It reads ticks from the supplied
// channel, skips the first graceTicks of them, then calls check on every
// subsequent tick. On a failed check it calls exit and returns. A canceled
// ctx returns without calling exit. Factored from StartWatcher so tests can
// drive ticks deterministically.
func runWatcher(
	ctx context.Context,
	ticks <-chan time.Time,
	graceTicks int,
	check func() checkResult,
	exit func(reason string),
) {
	elapsed := 0
	for {
		select {
		case <-ctx.Done():
			return
		case _, ok := <-ticks:
			if !ok {
				return
			}
			elapsed++
			if elapsed <= graceTicks {
				continue
			}
			res := check()
			if !res.ok {
				exit(res.reason)
				return
			}
		}
	}
}

// buildAncestors returns the set of PIDs containing self and every parent
// reachable through parents. The walk stops at PID 0, PID 4 (the Windows
// System process), a missing parent entry, a self-loop, or once
// maxAncestorDepth steps have been taken.
func buildAncestors(self uint32, parents map[uint32]uint32) map[uint32]struct{} {
	out := map[uint32]struct{}{}
	cur := self
	for i := 0; i < maxAncestorDepth; i++ {
		if cur == 0 || cur == 4 {
			break
		}
		if _, seen := out[cur]; seen {
			break
		}
		out[cur] = struct{}{}
		parent, ok := parents[cur]
		if !ok {
			break
		}
		cur = parent
	}
	return out
}

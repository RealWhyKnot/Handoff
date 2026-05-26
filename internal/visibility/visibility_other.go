// SPDX-License-Identifier: GPL-3.0-or-later
//go:build !windows

package visibility

import "context"

// StartWatcher is a no-op on non-Windows builds. The kill switch only
// matters on Windows; this stub exists so dev builds on macOS/Linux still
// compile.
func StartWatcher(ctx context.Context) {
	_ = ctx
}

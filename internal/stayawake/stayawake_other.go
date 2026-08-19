// SPDX-License-Identifier: GPL-3.0-or-later
//go:build !windows

package stayawake

import "context"

// Start is a no-op off Windows; sleep prevention only matters there.
func Start(ctx context.Context) {
	_ = ctx
}

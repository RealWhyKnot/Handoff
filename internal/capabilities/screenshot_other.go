// SPDX-License-Identifier: GPL-3.0-or-later
//go:build !windows

package capabilities

import (
	"context"
	"fmt"
)

func captureScreen(context.Context, screenshotOptions) (screenshot, error) {
	return screenshot{}, fmt.Errorf("sys.screenshot: only supported on Windows")
}

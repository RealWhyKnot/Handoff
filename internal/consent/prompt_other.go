// SPDX-License-Identifier: GPL-3.0-or-later
//go:build !windows

package consent

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"strings"
)

func SystemPrompt(ctx context.Context, req Request) (bool, error) {
	if err := ctx.Err(); err != nil {
		return false, err
	}
	fmt.Fprintln(os.Stderr, PromptText(req))
	fmt.Fprint(os.Stderr, "Allow? [y/N]: ")
	line, err := bufio.NewReader(os.Stdin).ReadString('\n')
	if err != nil {
		return false, err
	}
	line = strings.TrimSpace(strings.ToLower(line))
	return line == "y" || line == "yes", nil
}

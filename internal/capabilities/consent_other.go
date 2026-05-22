// SPDX-License-Identifier: GPL-3.0-or-later
//go:build !windows

package capabilities

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"strings"
)

func promptRiskConsent(ctx context.Context, req riskRequest) (bool, error) {
	if err := ctx.Err(); err != nil {
		return false, err
	}
	fmt.Fprintln(os.Stderr, riskPromptText(req))
	fmt.Fprint(os.Stderr, "Allow risky commands for this session? [y/N]: ")
	line, err := bufio.NewReader(os.Stdin).ReadString('\n')
	if err != nil {
		return false, err
	}
	line = strings.TrimSpace(strings.ToLower(line))
	return line == "y" || line == "yes", nil
}

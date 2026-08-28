// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"os"
	"path/filepath"

	"github.com/RealWhyKnot/Handoff/internal/supportlog"
)

func supportLogPath(opts *Options) string {
	if opts != nil && opts.LogDir != "" {
		return filepath.Join(opts.LogDir, "handoff.log")
	}
	path, err := supportlog.Path()
	if err != nil {
		return ""
	}
	return path
}

func auditDir(opts *Options) string {
	if opts != nil && opts.AuditDir != "" {
		return opts.AuditDir
	}
	root := os.Getenv("PROGRAMDATA")
	if root == "" {
		root = os.TempDir()
	}
	return filepath.Join(root, "whyknot", "handoff", "audit")
}

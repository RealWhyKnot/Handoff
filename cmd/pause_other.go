// SPDX-License-Identifier: GPL-3.0-or-later
//go:build !windows

package cmd

func ownsConsole() bool { return false }

func pauseIfOwnConsole() {}

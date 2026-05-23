// SPDX-License-Identifier: GPL-3.0-or-later
package main

import "testing"

func TestRunWithoutArgsShowsMenu(t *testing.T) {
	old := menuCommand
	defer func() { menuCommand = old }()

	called := false
	menuCommand = func(args []string) {
		called = true
		if len(args) != 0 {
			t.Fatalf("menu args = %#v, want empty", args)
		}
	}

	if code := run(nil); code != 0 {
		t.Fatalf("run(nil) = %d, want 0", code)
	}
	if !called {
		t.Fatal("menu command was not called")
	}
}

func TestRunNewExplicitStillCallsHostSession(t *testing.T) {
	old := newCommand
	defer func() { newCommand = old }()

	called := false
	newCommand = func(args []string) { called = true }

	if code := run([]string{"new"}); code != 0 {
		t.Fatalf("run(new) = %d, want 0", code)
	}
	if !called {
		t.Fatal("new command was not called for explicit `new` subcommand")
	}
}

func TestRunUnknownCommandReturnsUsageError(t *testing.T) {
	if code := run([]string{"nope"}); code != 2 {
		t.Fatalf("run(unknown) = %d, want 2", code)
	}
}

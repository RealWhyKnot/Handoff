// SPDX-License-Identifier: GPL-3.0-or-later
package main

import "testing"

func TestRunWithoutArgsStartsNewSession(t *testing.T) {
	old := newCommand
	defer func() { newCommand = old }()

	called := false
	newCommand = func(args []string) {
		called = true
		if len(args) != 0 {
			t.Fatalf("new args = %#v, want empty", args)
		}
	}

	if code := run(nil); code != 0 {
		t.Fatalf("run(nil) = %d, want 0", code)
	}
	if !called {
		t.Fatal("new command was not called")
	}
}

func TestRunUnknownCommandReturnsUsageError(t *testing.T) {
	if code := run([]string{"nope"}); code != 2 {
		t.Fatalf("run(unknown) = %d, want 2", code)
	}
}

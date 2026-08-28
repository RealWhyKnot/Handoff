// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"flag"
	"os"
	"path/filepath"
	"testing"
)

func TestRelayPrecedenceFlagBeatsEnvBeatsConfigBeatsDefault(t *testing.T) {
	cfgPath := filepath.Join(t.TempDir(), "config.json")
	if err := os.WriteFile(cfgPath, []byte(`{"relay":"https://config.example"}`), 0o644); err != nil {
		t.Fatal(err)
	}

	t.Run("default", func(t *testing.T) {
		t.Setenv("HANDOFF_RELAY", "")
		t.Setenv("HANDOFF_CONFIG", "")
		opts, _, err := parseOptions("test", nil, nil)
		if err != nil {
			t.Fatal(err)
		}
		if opts.Relay != defaultRelayURL {
			t.Fatalf("relay = %q, want the built-in default", opts.Relay)
		}
	})

	t.Run("config beats default", func(t *testing.T) {
		t.Setenv("HANDOFF_RELAY", "")
		t.Setenv("HANDOFF_CONFIG", cfgPath)
		opts, _, err := parseOptions("test", nil, nil)
		if err != nil {
			t.Fatal(err)
		}
		if opts.Relay != "https://config.example" {
			t.Fatalf("relay = %q, want the config value", opts.Relay)
		}
	})

	t.Run("env beats config", func(t *testing.T) {
		t.Setenv("HANDOFF_RELAY", "https://env.example")
		t.Setenv("HANDOFF_CONFIG", cfgPath)
		opts, _, err := parseOptions("test", nil, nil)
		if err != nil {
			t.Fatal(err)
		}
		if opts.Relay != "https://env.example" {
			t.Fatalf("relay = %q, want the environment value", opts.Relay)
		}
	})

	t.Run("flag beats env", func(t *testing.T) {
		t.Setenv("HANDOFF_RELAY", "https://env.example")
		t.Setenv("HANDOFF_CONFIG", cfgPath)
		opts, _, err := parseOptions("test", []string{"--relay", "https://flag.example"}, nil)
		if err != nil {
			t.Fatal(err)
		}
		if opts.Relay != "https://flag.example" {
			t.Fatalf("relay = %q, want the flag value", opts.Relay)
		}
	})
}

func TestTrailingSlashIsTrimmedFromRelay(t *testing.T) {
	t.Setenv("HANDOFF_CONFIG", "")
	opts, _, err := parseOptions("test", []string{"--relay", "https://example.test/"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if opts.Relay != "https://example.test" {
		t.Fatalf("relay = %q, want no trailing slash", opts.Relay)
	}
}

func TestInvalidRelayIsAUsageError(t *testing.T) {
	t.Setenv("HANDOFF_CONFIG", "")
	for _, bad := range []string{"not-a-url", "ftp://example.test", ""} {
		if _, _, err := parseOptions("test", []string{"--relay", bad}, nil); err == nil {
			t.Fatalf("relay %q was accepted", bad)
		}
	}
}

func TestMalformedConfigWarnsButDoesNotFail(t *testing.T) {
	cfgPath := filepath.Join(t.TempDir(), "broken.json")
	if err := os.WriteFile(cfgPath, []byte("{not json"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("HANDOFF_RELAY", "")
	t.Setenv("HANDOFF_CONFIG", cfgPath)

	opts, _, err := parseOptions("test", nil, nil)
	if err != nil {
		t.Fatalf("a broken config must not fail the command: %v", err)
	}
	if opts.Relay != defaultRelayURL {
		t.Fatalf("relay = %q, want the default after ignoring the config", opts.Relay)
	}
}

func TestMissingConfigIsSilent(t *testing.T) {
	t.Setenv("HANDOFF_RELAY", "")
	t.Setenv("HANDOFF_CONFIG", filepath.Join(t.TempDir(), "absent.json"))
	if _, _, err := parseOptions("test", nil, nil); err != nil {
		t.Fatalf("a missing config must not fail the command: %v", err)
	}
}

func TestRuntimeVersionOverrideIsGone(t *testing.T) {
	// HANDOFF_VERSION is a build-time variable. Reading it at runtime meant an
	// exported shell value silently changed what the updater compared against.
	t.Setenv("HANDOFF_VERSION", "9999.1.1.0")
	Version = "2026.8.18.0"
	if got := getCurrentVersion(); got != "2026.8.18.0" {
		t.Fatalf("getCurrentVersion() = %q, want the stamped version", got)
	}
}

func TestFlagsAreAcceptedAfterPositionalArguments(t *testing.T) {
	// Go's flag package stops parsing at the first non-flag, so
	// `exec <token> --list` used to treat --list as a command name and
	// `--arg limit=3` was silently dropped.
	t.Setenv("HANDOFF_CONFIG", "")
	t.Setenv("HANDOFF_RELAY", "")

	var list bool
	var pairs argList
	opts, rest, err := parseOptions("exec", []string{
		"n1_token", "app.list", "--arg", "limit=3", "--list", "--relay", "https://flag.example",
	}, func(fs *flag.FlagSet) {
		fs.BoolVar(&list, "list", false, "")
		fs.Var(&pairs, "arg", "")
	})
	if err != nil {
		t.Fatal(err)
	}
	if !list {
		t.Fatal("--list after a positional was not parsed")
	}
	if len(pairs) != 1 || pairs[0] != "limit=3" {
		t.Fatalf("--arg after a positional was not parsed: %v", pairs)
	}
	if opts.Relay != "https://flag.example" {
		t.Fatalf("relay = %q", opts.Relay)
	}
	if len(rest) != 2 || rest[0] != "n1_token" || rest[1] != "app.list" {
		t.Fatalf("positionals = %v, want the token and kind in order", rest)
	}
}

func TestDoubleDashStopsFlagParsing(t *testing.T) {
	t.Setenv("HANDOFF_CONFIG", "")
	_, rest, err := parseOptions("exec", []string{"tok", "--", "--not-a-flag"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(rest) != 2 || rest[1] != "--not-a-flag" {
		t.Fatalf("positionals = %v, want the literal after --", rest)
	}
}

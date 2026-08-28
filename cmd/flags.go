// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"encoding/json"
	"flag"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"strings"
)

const defaultRelayURL = "https://handoff.whyknot.dev"

// Options are the settings every subcommand shares. Precedence is flag, then
// environment, then config file, then the built-in default.
type Options struct {
	Relay     string
	JSON      bool
	Quiet     bool
	Verbose   bool
	LogDir    string
	AuditDir  string
	Config    string
	KeepAwake bool

	relaySet bool
}

type fileConfig struct {
	Relay     string `json:"relay"`
	LogDir    string `json:"log_dir"`
	AuditDir  string `json:"audit_dir"`
	KeepAwake *bool  `json:"keep_awake"`
}

// newFlagSet builds a flag set carrying the shared options. Usage goes to
// stdout so `handoff <cmd> --help > notes.txt` captures something.
func newFlagSet(name string, opts *Options) *flag.FlagSet {
	fs := flag.NewFlagSet(name, flag.ContinueOnError)
	fs.SetOutput(os.Stdout)
	fs.StringVar(&opts.Relay, "relay", "", "relay base URL")
	fs.BoolVar(&opts.JSON, "json", false, "print machine-readable output")
	fs.BoolVar(&opts.Quiet, "quiet", false, "print less")
	fs.BoolVar(&opts.Verbose, "verbose", false, "print more")
	fs.StringVar(&opts.LogDir, "log-dir", "", "directory for the support log")
	fs.StringVar(&opts.AuditDir, "audit-dir", "", "directory for the audit log")
	fs.StringVar(&opts.Config, "config", "", "path to a config file")
	return fs
}

// resolve applies precedence and validates. It never fails on a missing or
// unreadable config file; a broken config warns once and falls through.
func (o *Options) resolve() error {
	cfg := loadConfig(o.Config)

	if o.Relay == "" {
		o.Relay = firstNonEmpty(os.Getenv("HANDOFF_RELAY"), cfg.Relay, defaultRelayURL)
	}
	if o.LogDir == "" {
		o.LogDir = firstNonEmpty(os.Getenv("HANDOFF_LOG_DIR"), cfg.LogDir)
	}
	if o.AuditDir == "" {
		o.AuditDir = firstNonEmpty(os.Getenv("HANDOFF_AUDIT_DIR"), cfg.AuditDir)
	}

	o.KeepAwake = true
	if cfg.KeepAwake != nil {
		o.KeepAwake = *cfg.KeepAwake
	}

	o.Relay = strings.TrimRight(strings.TrimSpace(o.Relay), "/")
	u, err := url.Parse(o.Relay)
	if err != nil || u.Host == "" || (u.Scheme != "http" && u.Scheme != "https") {
		return fmt.Errorf("relay must be an http or https URL, got %q", o.Relay)
	}
	return nil
}

func loadConfig(explicit string) fileConfig {
	path := explicit
	if path == "" {
		path = os.Getenv("HANDOFF_CONFIG")
	}
	if path == "" {
		if dir, err := os.UserConfigDir(); err == nil {
			path = filepath.Join(dir, "whyknot", "handoff", "config.json")
		}
	}
	if path == "" {
		return fileConfig{}
	}

	data, err := os.ReadFile(path)
	if err != nil {
		if explicit != "" && !os.IsNotExist(err) {
			fmt.Fprintf(os.Stderr, "warning: could not read config %s: %v\n", path, err)
		}
		return fileConfig{}
	}

	var cfg fileConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		fmt.Fprintf(os.Stderr, "warning: ignoring malformed config %s: %v\n", path, err)
		return fileConfig{}
	}
	return cfg
}

// reorderFlagsFirst moves flags ahead of positional arguments. Go's flag
// package stops parsing at the first non-flag, so `exec <token> --list` would
// otherwise treat --list as a command name.
func reorderFlagsFirst(fs *flag.FlagSet, args []string) []string {
	takesValue := map[string]bool{}
	fs.VisitAll(func(f *flag.Flag) {
		switch f.Value.String() {
		case "false", "true":
			if _, isBool := f.Value.(interface{ IsBoolFlag() bool }); isBool {
				return
			}
		}
		takesValue[f.Name] = true
	})

	var flags, positional []string
	for i := 0; i < len(args); i++ {
		a := args[i]
		if a == "--" {
			positional = append(positional, args[i+1:]...)
			break
		}
		if !strings.HasPrefix(a, "-") || a == "-" {
			positional = append(positional, a)
			continue
		}

		flags = append(flags, a)
		name := strings.TrimLeft(a, "-")
		if strings.Contains(name, "=") {
			continue
		}
		if takesValue[name] && i+1 < len(args) {
			i++
			flags = append(flags, args[i])
		}
	}
	return append(flags, positional...)
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if strings.TrimSpace(v) != "" {
			return strings.TrimSpace(v)
		}
	}
	return ""
}

// parseOptions runs the shared flag set over args and returns the leftovers.
func parseOptions(name string, args []string, extra func(*flag.FlagSet)) (*Options, []string, error) {
	opts := &Options{}
	fs := newFlagSet(name, opts)
	if extra != nil {
		extra(fs)
	}
	if err := fs.Parse(reorderFlagsFirst(fs, args)); err != nil {
		return nil, nil, err
	}

	// An explicitly empty --relay is a mistake, usually an unset shell
	// variable. Falling through to the production default would quietly send
	// the session somewhere the caller did not ask for.
	fs.Visit(func(f *flag.Flag) {
		if f.Name == "relay" {
			opts.relaySet = true
		}
	})
	if opts.relaySet && strings.TrimSpace(opts.Relay) == "" {
		return nil, nil, fmt.Errorf("--relay was given but empty")
	}

	if err := opts.resolve(); err != nil {
		return nil, nil, err
	}
	return opts, fs.Args(), nil
}

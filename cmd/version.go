// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"
)

// parsedVersion is a WhyKnot YYYY.M.D.N version, optionally with a build
// suffix such as -22EE.
type parsedVersion struct {
	parts  [4]int
	suffix string
}

// parseVersion accepts YYYY.M.D.N with an optional -SUFFIX. "dev" and anything
// else unparseable returns ok=false, which callers treat as "not comparable".
func parseVersion(s string) (parsedVersion, bool) {
	var v parsedVersion
	s = strings.TrimSpace(s)
	s = strings.TrimPrefix(s, "v")
	if s == "" {
		return v, false
	}
	if i := strings.IndexByte(s, '-'); i >= 0 {
		v.suffix = s[i+1:]
		s = s[:i]
	}
	fields := strings.Split(s, ".")
	if len(fields) != 4 {
		return v, false
	}
	for i, f := range fields {
		n, err := strconv.Atoi(f)
		if err != nil || n < 0 {
			return v, false
		}
		v.parts[i] = n
	}
	return v, true
}

// compareVersions orders two versions. A build suffix sorts before the bare
// release of the same number, so 2026.8.18.0-22EE precedes 2026.8.18.0.
func compareVersions(a, b parsedVersion) int {
	for i := 0; i < 4; i++ {
		if a.parts[i] != b.parts[i] {
			if a.parts[i] < b.parts[i] {
				return -1
			}
			return 1
		}
	}
	switch {
	case a.suffix == b.suffix:
		return 0
	case a.suffix == "":
		return 1
	case b.suffix == "":
		return -1
	case a.suffix < b.suffix:
		return -1
	default:
		return 1
	}
}

// PrintVersion reports the running version plus where this build keeps its
// files, which is the first thing anyone needs when diagnosing a session.
func PrintVersion(args []string, version string) int {
	opts, _, err := parseOptions("version", args, nil)
	if err != nil {
		return 2
	}

	info := map[string]string{
		"version":   version,
		"relay":     opts.Relay,
		"log_path":  supportLogPath(opts),
		"audit_dir": auditDir(opts),
	}

	if opts.JSON {
		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "  ")
		if err := enc.Encode(info); err != nil {
			fmt.Fprintln(os.Stderr, "could not write json:", err)
			return 1
		}
		return 0
	}

	fmt.Println("handoff", version)
	fmt.Println("relay: ", info["relay"])
	fmt.Println("log:   ", info["log_path"])
	fmt.Println("audit: ", info["audit_dir"])
	return 0
}

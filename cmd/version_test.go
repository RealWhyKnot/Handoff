// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import "testing"

func TestParseVersion(t *testing.T) {
	cases := []struct {
		in    string
		ok    bool
		parts [4]int
		sfx   string
	}{
		{"2026.8.18.0", true, [4]int{2026, 8, 18, 0}, ""},
		{"v2026.8.18.0", true, [4]int{2026, 8, 18, 0}, ""},
		{"2026.8.18.0-22EE", true, [4]int{2026, 8, 18, 0}, "22EE"},
		{"2026.12.1.7", true, [4]int{2026, 12, 1, 7}, ""},
		{"dev", false, [4]int{}, ""},
		{"", false, [4]int{}, ""},
		{"2026.8.18", false, [4]int{}, ""},
		{"2026.8.18.x", false, [4]int{}, ""},
	}
	for _, c := range cases {
		got, ok := parseVersion(c.in)
		if ok != c.ok {
			t.Fatalf("parseVersion(%q) ok = %v, want %v", c.in, ok, c.ok)
		}
		if !ok {
			continue
		}
		if got.parts != c.parts || got.suffix != c.sfx {
			t.Fatalf("parseVersion(%q) = %+v, want %v/%q", c.in, got, c.parts, c.sfx)
		}
	}
}

func TestCompareVersions(t *testing.T) {
	mustParse := func(s string) parsedVersion {
		v, ok := parseVersion(s)
		if !ok {
			t.Fatalf("parseVersion(%q) failed", s)
		}
		return v
	}

	cases := []struct {
		a, b string
		want int
	}{
		{"2026.8.18.0", "2026.8.18.0", 0},
		{"2026.8.18.0", "2026.8.18.1", -1},
		{"2026.8.18.1", "2026.8.18.0", 1},
		{"2026.8.9.0", "2026.8.18.0", -1},
		{"2025.12.31.9", "2026.1.1.0", -1},
		// A build suffix precedes the bare release of the same number.
		{"2026.8.18.0-22EE", "2026.8.18.0", -1},
		{"2026.8.18.0", "2026.8.18.0-22EE", 1},
	}
	for _, c := range cases {
		if got := compareVersions(mustParse(c.a), mustParse(c.b)); got != c.want {
			t.Fatalf("compareVersions(%q, %q) = %d, want %d", c.a, c.b, got, c.want)
		}
	}
}

func TestShouldUpdate(t *testing.T) {
	cases := []struct {
		name    string
		current string
		latest  string
		force   bool
		want    bool
	}{
		{"newer available", "2026.8.18.0", "2026.8.19.0", false, true},
		{"same version", "2026.8.18.0", "2026.8.18.0", false, false},
		// Neither of these used to work: string equality meant a local build
		// always looked stale and an older manifest looked newer.
		{"local build stays put", "dev", "2026.8.19.0", false, false},
		{"local build with force", "dev", "2026.8.19.0", true, true},
		{"manifest is older", "2026.9.1.0", "2026.8.19.0", false, false},
		{"unreadable manifest", "2026.8.18.0", "garbage", false, false},
	}
	for _, c := range cases {
		if got := shouldUpdate(c.current, c.latest, c.force); got != c.want {
			t.Fatalf("%s: shouldUpdate(%q, %q, %v) = %v, want %v",
				c.name, c.current, c.latest, c.force, got, c.want)
		}
	}
}

// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

func TestParseTCPTestOptionsDefaultsAndClamps(t *testing.T) {
	opts, err := parseTCPTestOptions(rawArgs(t, map[string]interface{}{
		"target":     " example.com ",
		"port":       443,
		"timeout_ms": 20,
	}))
	if err != nil {
		t.Fatalf("parseTCPTestOptions err = %v", err)
	}
	if opts.target != "example.com" {
		t.Fatalf("target = %q, want example.com", opts.target)
	}
	if opts.port != 443 {
		t.Fatalf("port = %d, want 443", opts.port)
	}
	if opts.timeoutMS != 1000 {
		t.Fatalf("timeoutMS = %d, want lower clamp", opts.timeoutMS)
	}

	opts, err = parseTCPTestOptions(rawArgs(t, map[string]interface{}{
		"target":     "example.com",
		"port":       443,
		"timeout_ms": 60000,
	}))
	if err != nil {
		t.Fatalf("parseTCPTestOptions high timeout err = %v", err)
	}
	if opts.timeoutMS != 30000 {
		t.Fatalf("timeoutMS = %d, want upper clamp", opts.timeoutMS)
	}
}

func TestParseTCPTestOptionsRejectsBadInput(t *testing.T) {
	_, err := parseTCPTestOptions(rawArgs(t, map[string]interface{}{
		"target": "example.com; rm",
		"port":   443,
	}))
	if err == nil || !strings.Contains(err.Error(), "target") {
		t.Fatalf("target err = %v, want validation", err)
	}

	_, err = parseTCPTestOptions(rawArgs(t, map[string]interface{}{
		"target": "example.com",
		"port":   70000,
	}))
	if err == nil || !strings.Contains(err.Error(), "port") {
		t.Fatalf("port err = %v, want validation", err)
	}
}

func TestStorageDriveLetterArgNormalizesAndRejectsBadInput(t *testing.T) {
	got, err := storageDriveLetterArg(rawArgs(t, map[string]interface{}{
		"drive_letter": " c: ",
	}))
	if err != nil {
		t.Fatalf("storageDriveLetterArg err = %v", err)
	}
	if got != "C" {
		t.Fatalf("drive letter = %q, want C", got)
	}

	_, err = storageDriveLetterArg(rawArgs(t, map[string]interface{}{
		"drive_letter": "System",
	}))
	if err == nil || !strings.Contains(err.Error(), "single letter") {
		t.Fatalf("storageDriveLetterArg err = %v, want validation", err)
	}
}

func TestPowerUserBoundedArgs(t *testing.T) {
	if got := resourceTopArg(rawArgs(t, map[string]interface{}{"top": 500})); got != 50 {
		t.Fatalf("resourceTopArg = %d, want 50", got)
	}
	if got := resourceTopArg(rawArgs(t, map[string]interface{}{"top": 0})); got != 10 {
		t.Fatalf("resourceTopArg zero = %d, want default", got)
	}
	if got := startupMaxResultsArg(rawArgs(t, map[string]interface{}{"limit": 5000})); got != 2000 {
		t.Fatalf("startupMaxResultsArg = %d, want 2000", got)
	}
	if got := startupMaxResultsArg(rawArgs(t, map[string]interface{}{"limit": -1})); got != 300 {
		t.Fatalf("startupMaxResultsArg negative = %d, want default", got)
	}

	shares := parseNetSharesOptions(rawArgs(t, map[string]interface{}{
		"include_hidden":   true,
		"include_sessions": false,
		"max_results":      5000,
	}))
	if !shares.includeHidden || shares.includeSessions || shares.maxResults != 1000 {
		t.Fatalf("parseNetSharesOptions = %#v, want hidden=true sessions=false max=1000", shares)
	}
}

func TestRegisterAllIncludesPowerUserKinds(t *testing.T) {
	r := dispatch.New()
	RegisterAll(r, nil)
	kinds := map[string]bool{}
	for _, kind := range r.Kinds() {
		kinds[kind] = true
	}
	for _, want := range []string{
		"net.tcp-test",
		"storage.volumes",
		"sys.resources",
		"startup.list",
		"net.shares",
		"sec.local-admins",
	} {
		if !kinds[want] {
			t.Fatalf("registered kinds missing %s in %#v", want, r.Kinds())
		}
	}
}

func TestPowerUserPowerShellCapabilitiesSmoke(t *testing.T) {
	if runtime.GOOS != "windows" {
		t.Skip("PowerShell-backed smoke test requires Windows")
	}

	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	cases := []struct {
		name string
		fn   func(context.Context, map[string]json.RawMessage) (interface{}, error)
		args map[string]json.RawMessage
	}{
		{"storage.volumes", storageVolumes, rawArgs(t, map[string]interface{}{"drive_letter": "C"})},
		{"sys.resources", sysResources, rawArgs(t, map[string]interface{}{"top": 3})},
		{"startup.list", startupList, rawArgs(t, map[string]interface{}{"max_results": 5})},
		{"sec.local-admins", secLocalAdmins, rawArgs(t, map[string]interface{}{})},
		{"net.shares", netShares, rawArgs(t, map[string]interface{}{"max_results": 5})},
	}
	for _, tc := range cases {
		res, err := tc.fn(ctx, tc.args)
		if err != nil {
			t.Fatalf("%s err = %v", tc.name, err)
		}
		if res == nil {
			t.Fatalf("%s returned nil result", tc.name)
		}
	}
}

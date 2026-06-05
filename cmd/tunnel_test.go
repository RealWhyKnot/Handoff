// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"strings"
	"testing"
)

func TestParseTunnelArgsRequiresToken(t *testing.T) {
	if _, err := parseTunnelArgs(nil); err == nil {
		t.Fatal("expected missing token to error")
	}
}

func TestParseTunnelArgsAcceptsTokenAndPort(t *testing.T) {
	opts, err := parseTunnelArgs([]string{"tk_abc", "--local-port", "1234"})
	if err != nil {
		t.Fatalf("parseTunnelArgs: %v", err)
	}
	if opts.token != "tk_abc" {
		t.Fatalf("token = %q, want tk_abc", opts.token)
	}
	if opts.localPort != 1234 {
		t.Fatalf("localPort = %d, want 1234", opts.localPort)
	}
}

func TestParseTunnelArgsAcceptsEqualsForm(t *testing.T) {
	opts, err := parseTunnelArgs([]string{"tk_abc", "--local-port=5555", "--relay=https://example.invalid"})
	if err != nil {
		t.Fatalf("parseTunnelArgs: %v", err)
	}
	if opts.localPort != 5555 {
		t.Fatalf("localPort = %d, want 5555", opts.localPort)
	}
	if opts.relay != "https://example.invalid" {
		t.Fatalf("relay = %q", opts.relay)
	}
}

func TestParseTunnelArgsRejectsBadPort(t *testing.T) {
	for _, bad := range []string{"0", "70000", "notanumber"} {
		if _, err := parseTunnelArgs([]string{"tk", "--local-port", bad}); err == nil {
			t.Fatalf("expected --local-port %q to error", bad)
		}
	}
}

func TestParseTunnelArgsRejectsUnknownFlag(t *testing.T) {
	if _, err := parseTunnelArgs([]string{"tk", "--nope"}); err == nil {
		t.Fatal("expected unknown flag to error")
	}
}

func TestParseTunnelArgsDefaultsLocalPort(t *testing.T) {
	opts, err := parseTunnelArgs([]string{"tk_only"})
	if err != nil {
		t.Fatalf("parseTunnelArgs: %v", err)
	}
	if opts.localPort != 47800 {
		t.Fatalf("default localPort = %d, want 47800", opts.localPort)
	}
}

func TestTunnelWsURLBuildsRelayPath(t *testing.T) {
	got, err := tunnelWsURL("https://handoff.example/", "tk_abc")
	if err != nil {
		t.Fatalf("tunnelWsURL: %v", err)
	}
	if got != "wss://handoff.example/api/tunnel/tk_abc" {
		t.Fatalf("ws url = %q", got)
	}
}

func TestTunnelWsURLAcceptsHttpRelay(t *testing.T) {
	got, err := tunnelWsURL("http://localhost:5099", "tk_abc")
	if err != nil {
		t.Fatalf("tunnelWsURL: %v", err)
	}
	if got != "ws://localhost:5099/api/tunnel/tk_abc" {
		t.Fatalf("ws url = %q", got)
	}
}

func TestTunnelWsURLAcceptsFullURL(t *testing.T) {
	got, err := tunnelWsURL("https://ignored", "https://handoff.example/api/tunnel/tk_xyz")
	if err != nil {
		t.Fatalf("tunnelWsURL: %v", err)
	}
	if !strings.HasPrefix(got, "wss://handoff.example/") {
		t.Fatalf("ws url = %q", got)
	}
}

func TestTunnelWsURLAcceptsFullWebSocketURL(t *testing.T) {
	got, err := tunnelWsURL("https://ignored", "wss://handoff.example/api/tunnel/n2_tk_xyz")
	if err != nil {
		t.Fatalf("tunnelWsURL: %v", err)
	}
	if got != "wss://handoff.example/api/tunnel/n2_tk_xyz" {
		t.Fatalf("ws url = %q", got)
	}
}

func TestNewStreamIDFormat(t *testing.T) {
	id := newStreamID()
	if !strings.HasPrefix(id, "s_") {
		t.Fatalf("stream id = %q, want s_ prefix", id)
	}
	if len(id) < 6 {
		t.Fatalf("stream id too short: %q", id)
	}
}

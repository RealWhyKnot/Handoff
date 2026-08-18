// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"context"
	"encoding/json"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"
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
	opts, err := parseTunnelArgs([]string{"tk_abc", "--local-port=5555", "--relay=https://example.invalid", "--http-host=fritz.repeater"})
	if err != nil {
		t.Fatalf("parseTunnelArgs: %v", err)
	}
	if opts.localPort != 5555 {
		t.Fatalf("localPort = %d, want 5555", opts.localPort)
	}
	if opts.relay != "https://example.invalid" {
		t.Fatalf("relay = %q", opts.relay)
	}
	if opts.httpHost != "fritz.repeater" {
		t.Fatalf("httpHost = %q", opts.httpHost)
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

func TestParseTunnelArgsRejectsBadHTTPHost(t *testing.T) {
	if _, err := parseTunnelArgs([]string{"tk", "--http-host", "bad\r\nHost: evil"}); err == nil {
		t.Fatal("expected bad --http-host to error")
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
	if opts.portExplicit {
		t.Fatal("default port must not be marked explicit")
	}
}

func TestParseTunnelArgsMarksExplicitPort(t *testing.T) {
	opts, err := parseTunnelArgs([]string{"tk", "--local-port", "1234"})
	if err != nil {
		t.Fatalf("parseTunnelArgs: %v", err)
	}
	if !opts.portExplicit {
		t.Fatal("explicit --local-port was not recorded")
	}
}

// A busy default port must roll forward instead of killing the process.
func TestBindLocalListenerRollsForwardWhenPortBusy(t *testing.T) {
	busy, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer busy.Close()
	busyPort := busy.Addr().(*net.TCPAddr).Port

	listener, err := bindLocalListener(busyPort, false)
	if err != nil {
		t.Fatalf("bindLocalListener: %v", err)
	}
	defer listener.Close()
	if got := listener.Addr().(*net.TCPAddr).Port; got == busyPort {
		t.Fatalf("bound the busy port %d", got)
	}
}

// An explicitly requested port must fail loudly rather than silently moving.
func TestBindLocalListenerRespectsExplicitPort(t *testing.T) {
	busy, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer busy.Close()
	busyPort := busy.Addr().(*net.TCPAddr).Port

	if listener, err := bindLocalListener(busyPort, true); err == nil {
		listener.Close()
		t.Fatal("explicit busy port should not bind")
	}
}

func TestBindLocalListenerBindsRequestedPortWhenFree(t *testing.T) {
	probe, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	port := probe.Addr().(*net.TCPAddr).Port
	probe.Close()

	listener, err := bindLocalListener(port, true)
	if err != nil {
		t.Skipf("port %d not reusable in this environment: %v", port, err)
	}
	defer listener.Close()
	if got := listener.Addr().(*net.TCPAddr).Port; got != port {
		t.Fatalf("bound port %d, want %d", got, port)
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

func TestDefaultTunnelHTTPHostKeepsHostname(t *testing.T) {
	if got := defaultTunnelHTTPHost("fritz.repeater", 80); got != "fritz.repeater" {
		t.Fatalf("defaultTunnelHTTPHost = %q", got)
	}
	if got := defaultTunnelHTTPHost("127.0.0.1", 80); got != "" {
		t.Fatalf("loopback defaultTunnelHTTPHost = %q, want empty", got)
	}
	if got := defaultTunnelHTTPHost("192.168.2.103", 8080); got != "192.168.2.103:8080" {
		t.Fatalf("port defaultTunnelHTTPHost = %q", got)
	}
}

func TestRewriteHTTPHostHeaderReplacesHost(t *testing.T) {
	req := []byte("GET / HTTP/1.1\r\nHost: 127.0.0.1:18180\r\nUser-Agent: test\r\n\r\n")
	got, ready := rewriteHTTPHostHeader(req, "fritz.repeater")
	if !ready {
		t.Fatal("rewriteHTTPHostHeader was not ready")
	}
	want := "GET / HTTP/1.1\r\nHost: fritz.repeater\r\nUser-Agent: test\r\n\r\n"
	if string(got) != want {
		t.Fatalf("rewritten request = %q, want %q", got, want)
	}
}

func TestRewriteHTTPHostHeaderInsertsHost(t *testing.T) {
	req := []byte("GET / HTTP/1.1\r\nUser-Agent: test\r\n\r\n")
	got, ready := rewriteHTTPHostHeader(req, "fritz.repeater")
	if !ready {
		t.Fatal("rewriteHTTPHostHeader was not ready")
	}
	want := "GET / HTTP/1.1\r\nHost: fritz.repeater\r\nUser-Agent: test\r\n\r\n"
	if string(got) != want {
		t.Fatalf("rewritten request = %q, want %q", got, want)
	}
}

func TestRewriteHTTPHostHeaderWaitsForCompleteHeader(t *testing.T) {
	if got, ready := rewriteHTTPHostHeader([]byte("GET / HTTP/1.1\r\nHost: 127"), "fritz.repeater"); ready || got != nil {
		t.Fatalf("rewriteHTTPHostHeader ready=%v got=%q, want wait", ready, got)
	}
}

func TestRewriteHTTPHostHeaderPassesThroughNonHTTP(t *testing.T) {
	input := []byte{0x16, 0x03, 0x01, 0x00}
	got, ready := rewriteHTTPHostHeader(input, "fritz.repeater")
	if !ready {
		t.Fatal("non-HTTP data should be ready")
	}
	if string(got) != string(input) {
		t.Fatalf("non-HTTP data changed: %q", got)
	}
}

// A relay that drops the connection must not end the operator's tunnel: the
// listener stays bound and the client dials again.
func TestTunnelSessionsReconnectAfterRelayDrop(t *testing.T) {
	var dials atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		n := dials.Add(1)
		ready, _ := json.Marshal(map[string]interface{}{
			"kind": "tunnel_ready", "tunnel_id": "tn1", "host_addr": "127.0.0.1", "host_port": 80,
		})
		_ = conn.Write(r.Context(), websocket.MessageText, ready)
		if n < 3 {
			// Simulate an outage: hang up right after the handshake.
			conn.Close(websocket.StatusInternalError, "drop")
			return
		}
		<-r.Context().Done()
		conn.Close(websocket.StatusNormalClosure, "bye")
	}))
	defer server.Close()

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer listener.Close()

	wsURL := "ws" + strings.TrimPrefix(server.URL, "http") + "/api/tunnel/tk"
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	done := make(chan struct{})
	go func() {
		runTunnelSessions(ctx, tunnelOptions{token: "tk"}, wsURL, listener,
			listener.Addr().(*net.TCPAddr).Port)
		close(done)
	}()

	deadline := time.After(10 * time.Second)
	for dials.Load() < 3 {
		select {
		case <-deadline:
			t.Fatalf("relay saw %d dials, want 3 (no reconnect)", dials.Load())
		case <-time.After(20 * time.Millisecond):
		}
	}

	// The listener must still be usable after those reconnects.
	probe, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatalf("listener stopped serving after reconnect: %v", err)
	}
	probe.Close()

	cancel()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("runTunnelSessions did not stop on context cancel")
	}
}

// A relay-side tunnel_closed is terminal -- no reconnect storm.
func TestTunnelSessionsStopOnRelayTunnelClosed(t *testing.T) {
	var dials atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		dials.Add(1)
		ready, _ := json.Marshal(map[string]interface{}{
			"kind": "tunnel_ready", "tunnel_id": "tn1", "host_addr": "127.0.0.1", "host_port": 80,
		})
		_ = conn.Write(r.Context(), websocket.MessageText, ready)
		closed, _ := json.Marshal(map[string]interface{}{"kind": "tunnel_closed", "reason": "host ended session"})
		_ = conn.Write(r.Context(), websocket.MessageText, closed)
		<-r.Context().Done()
	}))
	defer server.Close()

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer listener.Close()

	wsURL := "ws" + strings.TrimPrefix(server.URL, "http") + "/api/tunnel/tk"
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	done := make(chan struct{})
	go func() {
		runTunnelSessions(ctx, tunnelOptions{token: "tk"}, wsURL, listener,
			listener.Addr().(*net.TCPAddr).Port)
		close(done)
	}()

	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("tunnel_closed did not end the session loop")
	}
	if got := dials.Load(); got != 1 {
		t.Fatalf("dials = %d, want 1 (no reconnect after tunnel_closed)", got)
	}
}

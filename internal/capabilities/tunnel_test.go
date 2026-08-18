// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"bytes"
	"context"
	"encoding/base64"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

type recordedTunnelData struct {
	tunnelID string
	streamID string
	payload  []byte
}

type fakeTunnelBridge struct {
	mu     sync.Mutex
	data   []recordedTunnelData
	closes []string
}

func newFakeTunnelBridge() *fakeTunnelBridge {
	return &fakeTunnelBridge{}
}

func (b *fakeTunnelBridge) SendTunnelData(_ context.Context, tunnelID, streamID string, data []byte) error {
	cp := make([]byte, len(data))
	copy(cp, data)
	b.mu.Lock()
	b.data = append(b.data, recordedTunnelData{tunnelID: tunnelID, streamID: streamID, payload: cp})
	b.mu.Unlock()
	return nil
}

func (b *fakeTunnelBridge) SendTunnelStreamClose(_ context.Context, tunnelID, streamID, reason string) error {
	b.mu.Lock()
	b.closes = append(b.closes, tunnelID+":"+streamID+":"+reason)
	b.mu.Unlock()
	return nil
}

func (b *fakeTunnelBridge) SendTunnelClose(_ context.Context, tunnelID, reason string) error {
	b.mu.Lock()
	b.closes = append(b.closes, tunnelID+":TUNNEL:"+reason)
	b.mu.Unlock()
	return nil
}

// streamBytes returns everything the host has sent back to the operator for one
// stream so far, in order.
func (b *fakeTunnelBridge) streamBytes(tunnelID, streamID string) []byte {
	b.mu.Lock()
	defer b.mu.Unlock()
	var out []byte
	for _, rec := range b.data {
		if rec.tunnelID == tunnelID && rec.streamID == streamID {
			out = append(out, rec.payload...)
		}
	}
	return out
}

func (b *fakeTunnelBridge) closeReasons() []string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return append([]string(nil), b.closes...)
}

// waitEcho polls the returned bytes until wantLen have come back or the deadline.
func waitEcho(t *testing.T, b *fakeTunnelBridge, tunnelID, streamID string, wantLen int) []byte {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for {
		got := b.streamBytes(tunnelID, streamID)
		if len(got) >= wantLen {
			return got
		}
		if time.Now().After(deadline) {
			t.Fatalf("only %d of %d bytes echoed back for %s/%s", len(got), wantLen, tunnelID, streamID)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func openStreamOrdered(mgr *tunnelMgr, tunnelID, streamID string) {
	mgr.enqueueFrame("tunnel.stream_open", tunnelID, streamID, "", "")
}

func writeOrdered(mgr *tunnelMgr, tunnelID, streamID string, payload []byte) {
	mgr.enqueueFrame("tunnel.data", tunnelID, streamID, base64.StdEncoding.EncodeToString(payload), "")
}

func startEchoServer(t *testing.T) (int, func()) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go func(c net.Conn) {
				defer c.Close()
				_, _ = io.Copy(c, c)
			}(conn)
		}
	}()
	port := listener.Addr().(*net.TCPAddr).Port
	return port, func() { _ = listener.Close() }
}

func startHTTPServer(t *testing.T) (int, func()) {
	t.Helper()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/plain")
		w.Header().Set("Connection", "close")
		_, _ = w.Write([]byte("fritz ok"))
	}))
	u, err := url.Parse(server.URL)
	if err != nil {
		server.Close()
		t.Fatalf("parse server URL: %v", err)
	}
	_, portText, err := net.SplitHostPort(u.Host)
	if err != nil {
		server.Close()
		t.Fatalf("split server host: %v", err)
	}
	port, err := strconv.Atoi(portText)
	if err != nil {
		server.Close()
		t.Fatalf("parse server port: %v", err)
	}
	return port, server.Close
}

func waitStreamCount(t *testing.T, tun *tunnelState, want int) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for {
		tun.mu.Lock()
		n := len(tun.streams)
		tun.mu.Unlock()
		if n == want {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("stream count = %d, want %d", n, want)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func TestTunnelManagerOpensAndForwardsBytes(t *testing.T) {
	port, stop := startEchoServer(t)
	defer stop()

	bridge := newFakeTunnelBridge()
	mgr := newTunnelMgr(bridge)
	defer mgr.shutdown()

	if _, err := mgr.open("tn1", port, "127.0.0.1"); err != nil {
		t.Fatalf("open: %v", err)
	}
	openStreamOrdered(mgr, "tn1", "s1")
	payload := []byte("hello tunnel\n")
	writeOrdered(mgr, "tn1", "s1", payload)

	got := waitEcho(t, bridge, "tn1", "s1", len(payload))
	if string(got) != string(payload) {
		t.Fatalf("echo = %q, want %q", got, payload)
	}
}

// TestTunnelManagerPreservesDataOrder is the core correctness guarantee: many
// data frames on one stream must reach the socket in wire order. A single
// ordered consumer makes this hold; the old goroutine-per-frame path did not.
func TestTunnelManagerPreservesDataOrder(t *testing.T) {
	port, stop := startEchoServer(t)
	defer stop()

	bridge := newFakeTunnelBridge()
	mgr := newTunnelMgr(bridge)
	defer mgr.shutdown()

	if _, err := mgr.open("tn1", port, "127.0.0.1"); err != nil {
		t.Fatalf("open: %v", err)
	}
	openStreamOrdered(mgr, "tn1", "s1")

	const frames = 200
	var want bytes.Buffer
	for i := 0; i < frames; i++ {
		chunk := fmt.Sprintf("%06d", i)
		want.WriteString(chunk)
		writeOrdered(mgr, "tn1", "s1", []byte(chunk))
	}

	got := waitEcho(t, bridge, "tn1", "s1", want.Len())
	if !bytes.Equal(got[:want.Len()], want.Bytes()) {
		t.Fatalf("frames arrived out of order")
	}
}

func TestTunnelManagerForwardsHTTPResponse(t *testing.T) {
	port, stop := startHTTPServer(t)
	defer stop()

	bridge := newFakeTunnelBridge()
	mgr := newTunnelMgr(bridge)
	defer mgr.shutdown()

	if _, err := mgr.open("tn1", port, "127.0.0.1"); err != nil {
		t.Fatalf("open: %v", err)
	}
	openStreamOrdered(mgr, "tn1", "s1")
	writeOrdered(mgr, "tn1", "s1", []byte("GET /login_sid.lua HTTP/1.1\r\nHost: fritz.repeater\r\nConnection: close\r\n\r\n"))

	deadline := time.After(3 * time.Second)
	for {
		if bytes.Contains(bridge.streamBytes("tn1", "s1"), []byte("fritz ok")) {
			return
		}
		select {
		case <-deadline:
			t.Fatalf("HTTP response did not arrive; got %q", bridge.streamBytes("tn1", "s1"))
		case <-time.After(10 * time.Millisecond):
		}
	}
}

func TestTunnelManagerRejectsNonLoopback(t *testing.T) {
	mgr := newTunnelMgr(newFakeTunnelBridge())
	if _, err := mgr.open("tn1", 5555, "8.8.8.8"); err == nil {
		t.Fatal("expected public address to be rejected")
	}
}

func TestTunnelManagerAcceptsPrivateTarget(t *testing.T) {
	mgr := newTunnelMgr(newFakeTunnelBridge())
	tun, err := mgr.open("tn1", 5555, "192.168.2.1")
	if err != nil {
		t.Fatalf("open private target: %v", err)
	}
	if tun.localHost != "192.168.2.1" || tun.dialHost != "192.168.2.1" {
		t.Fatalf("localHost=%q dialHost=%q, want 192.168.2.1 for both", tun.localHost, tun.dialHost)
	}
}

// TestTunnelManagerPinsResolvedHostname: the hostname is kept for display but the
// dial address is pinned to the IP validated at open time (closes the TOCTOU gap).
func TestTunnelManagerPinsResolvedHostname(t *testing.T) {
	oldLookup := tunnelLookupIP
	tunnelLookupIP = func(_ context.Context, host string) ([]net.IPAddr, error) {
		if host != "fritz.repeater" {
			t.Fatalf("lookup host = %q, want fritz.repeater", host)
		}
		return []net.IPAddr{{IP: net.ParseIP("192.168.2.103")}}, nil
	}
	t.Cleanup(func() { tunnelLookupIP = oldLookup })

	mgr := newTunnelMgr(newFakeTunnelBridge())
	tun, err := mgr.open("tn1", 5555, "fritz.repeater")
	if err != nil {
		t.Fatalf("open hostname: %v", err)
	}
	if tun.localHost != "fritz.repeater" {
		t.Fatalf("localHost = %q, want fritz.repeater", tun.localHost)
	}
	if tun.dialHost != "192.168.2.103" {
		t.Fatalf("dialHost = %q, want pinned 192.168.2.103", tun.dialHost)
	}
}

func TestTunnelManagerRejectsHostnameResolvingPublic(t *testing.T) {
	oldLookup := tunnelLookupIP
	tunnelLookupIP = func(context.Context, string) ([]net.IPAddr, error) {
		return []net.IPAddr{{IP: net.ParseIP("8.8.8.8")}}, nil
	}
	t.Cleanup(func() { tunnelLookupIP = oldLookup })

	mgr := newTunnelMgr(newFakeTunnelBridge())
	if _, err := mgr.open("tn1", 5555, "example.com"); err == nil {
		t.Fatal("expected public hostname to be rejected")
	}
}

func TestTunnelManagerRejectsBadPort(t *testing.T) {
	mgr := newTunnelMgr(newFakeTunnelBridge())
	if _, err := mgr.open("tn1", 0, "127.0.0.1"); err == nil {
		t.Fatal("expected port 0 to be rejected")
	}
	if _, err := mgr.open("tn1", 70000, "127.0.0.1"); err == nil {
		t.Fatal("expected port 70000 to be rejected")
	}
}

func TestIsAllowedTunnelTargetIP(t *testing.T) {
	allow := []string{"127.0.0.1", "::1", "192.168.1.1", "10.0.0.5", "172.16.0.1", "169.254.1.1", "100.64.0.1", "100.127.255.254"}
	deny := []string{"8.8.8.8", "1.1.1.1", "100.128.0.1", "100.63.255.255", "0.0.0.0", "224.0.0.1"}
	for _, s := range allow {
		if !isAllowedTunnelTargetIP(net.ParseIP(s)) {
			t.Errorf("isAllowedTunnelTargetIP(%s) = false, want true", s)
		}
	}
	for _, s := range deny {
		if isAllowedTunnelTargetIP(net.ParseIP(s)) {
			t.Errorf("isAllowedTunnelTargetIP(%s) = true, want false", s)
		}
	}
}

func TestTunnelTargetDescription(t *testing.T) {
	if d := tunnelTargetDescription("", 8080); !strings.Contains(d, "this computer") {
		t.Fatalf("loopback description = %q, want to mention this computer", d)
	}
	if d := tunnelTargetDescription("127.0.0.1", 80); !strings.Contains(d, "this computer") {
		t.Fatalf("loopback description = %q, want to mention this computer", d)
	}
	if d := tunnelTargetDescription("192.168.1.1", 80); !strings.Contains(d, "local network") {
		t.Fatalf("LAN description = %q, want to mention local network", d)
	}
}

func TestTunnelDataOversizeClosesStream(t *testing.T) {
	port, stop := startEchoServer(t)
	defer stop()
	bridge := newFakeTunnelBridge()
	mgr := newTunnelMgr(bridge)
	defer mgr.shutdown()
	if _, err := mgr.open("tn1", port, "127.0.0.1"); err != nil {
		t.Fatalf("open: %v", err)
	}
	openStreamOrdered(mgr, "tn1", "s1")
	big := make([]byte, tunnelMaxFramePayload+1)
	writeOrdered(mgr, "tn1", "s1", big)

	deadline := time.Now().Add(2 * time.Second)
	for {
		found := false
		for _, c := range bridge.closeReasons() {
			if strings.Contains(c, "frame too large") {
				found = true
			}
		}
		if found {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("oversize frame did not close the stream; closes=%v", bridge.closeReasons())
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestTunnelBadBase64DoesNotKillStream(t *testing.T) {
	port, stop := startEchoServer(t)
	defer stop()
	bridge := newFakeTunnelBridge()
	mgr := newTunnelMgr(bridge)
	defer mgr.shutdown()
	if _, err := mgr.open("tn1", port, "127.0.0.1"); err != nil {
		t.Fatalf("open: %v", err)
	}
	openStreamOrdered(mgr, "tn1", "s1")
	mgr.enqueueFrame("tunnel.data", "tn1", "s1", "$$$not base64!!!", "") // dropped, must not crash
	writeOrdered(mgr, "tn1", "s1", []byte("still alive"))

	got := waitEcho(t, bridge, "tn1", "s1", len("still alive"))
	if string(got) != "still alive" {
		t.Fatalf("echo = %q, want %q", got, "still alive")
	}
}

func TestTunnelOpenHandlerRequiresConsent(t *testing.T) {
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return false, nil })
	bridge := newFakeTunnelBridge()
	tunnelMu.Lock()
	tunnelManager = newTunnelMgr(bridge)
	tunnelMu.Unlock()
	defer func() {
		tunnelMu.Lock()
		tunnelManager.shutdown()
		tunnelManager = nil
		tunnelMu.Unlock()
	}()

	_, err := tunnelOpenHandler(context.Background(), rawArgs(t, map[string]interface{}{
		"tunnel_id":  "tn1",
		"local_port": 8080,
	}))
	if !errors.Is(err, errRiskDenied) {
		t.Fatalf("tunnelOpen err = %v, want errRiskDenied", err)
	}
}

// TestTunnelEndToEndThroughGlobalManager exercises the production path: open via
// the handler, then drive frames through EnqueueTunnelFrame (as the receive loop
// does), then close.
func TestTunnelEndToEndThroughGlobalManager(t *testing.T) {
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return true, nil })
	port, stop := startEchoServer(t)
	defer stop()

	bridge := newFakeTunnelBridge()
	tunnelMu.Lock()
	tunnelManager = newTunnelMgr(bridge)
	tunnelMu.Unlock()
	defer func() {
		tunnelMu.Lock()
		tunnelManager.shutdown()
		tunnelManager = nil
		tunnelMu.Unlock()
	}()

	if _, err := tunnelOpenHandler(context.Background(), rawArgs(t, map[string]interface{}{
		"tunnel_id":  "tn1",
		"local_port": port,
	})); err != nil {
		t.Fatalf("tunnelOpen: %v", err)
	}
	EnqueueTunnelFrame("tunnel.stream_open", rawArgs(t, map[string]interface{}{
		"tunnel_id": "tn1", "stream_id": "s1",
	}))
	payload := []byte("round trip bytes")
	EnqueueTunnelFrame("tunnel.data", rawArgs(t, map[string]interface{}{
		"tunnel_id": "tn1", "stream_id": "s1", "data_base64": base64.StdEncoding.EncodeToString(payload),
	}))

	got := waitEcho(t, bridge, "tn1", "s1", len(payload))
	if string(got) != string(payload) {
		t.Fatalf("payload = %q, want %q", got, payload)
	}

	EnqueueTunnelFrame("tunnel.stream_close", rawArgs(t, map[string]interface{}{
		"tunnel_id": "tn1", "stream_id": "s1",
	}))
	res, err := tunnelCloseHandler(context.Background(), rawArgs(t, map[string]interface{}{
		"tunnel_id": "tn1",
	}))
	if err != nil {
		t.Fatalf("tunnelClose: %v", err)
	}
	if got := res.(map[string]interface{})["closed"]; got != true {
		t.Fatalf("tunnel close result = %v, want closed=true", res)
	}
}

func TestTunnelManagerShutdownClosesActiveStreams(t *testing.T) {
	port, stop := startEchoServer(t)
	defer stop()
	mgr := newTunnelMgr(newFakeTunnelBridge())
	tun, err := mgr.open("tn1", port, "127.0.0.1")
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	openStreamOrdered(mgr, "tn1", "s1")
	waitStreamCount(t, tun, 1)

	mgr.shutdown()
	if _, ok := mgr.get("tn1"); ok {
		t.Fatal("tunnel still present after shutdown")
	}
}

func TestTunnelManagerOpenWithExistingTunnelIDReturnsExisting(t *testing.T) {
	mgr := newTunnelMgr(newFakeTunnelBridge())
	defer mgr.shutdown()
	first, err := mgr.open("tn1", 8080, "127.0.0.1")
	if err != nil {
		t.Fatalf("open first: %v", err)
	}
	second, err := mgr.open("tn1", 8080, "127.0.0.1")
	if err != nil {
		t.Fatalf("open second: %v", err)
	}
	if first != second {
		t.Fatal("expected idempotent open to return the same state")
	}
	if _, err := mgr.open("tn1", 9090, "127.0.0.1"); err == nil {
		t.Fatal("expected port mismatch to reject reuse of tunnel_id")
	}
}

func TestTunnelMaxFramePayloadIsAtLeast64KB(t *testing.T) {
	if tunnelMaxFramePayload < 64*1024 {
		t.Fatalf("tunnelMaxFramePayload = %d, want at least 64KB", tunnelMaxFramePayload)
	}
}

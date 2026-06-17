// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/base64"
	"errors"
	"io"
	"net"
	"strconv"
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
	dataCh chan recordedTunnelData
}

func newFakeTunnelBridge() *fakeTunnelBridge {
	return &fakeTunnelBridge{dataCh: make(chan recordedTunnelData, 16)}
}

func (b *fakeTunnelBridge) SendTunnelData(_ context.Context, tunnelID, streamID string, data []byte) error {
	cp := make([]byte, len(data))
	copy(cp, data)
	rec := recordedTunnelData{tunnelID: tunnelID, streamID: streamID, payload: cp}
	b.mu.Lock()
	b.data = append(b.data, rec)
	b.mu.Unlock()
	select {
	case b.dataCh <- rec:
	default:
	}
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

func TestTunnelManagerOpensAndForwardsBytes(t *testing.T) {
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return true, nil })
	port, stop := startEchoServer(t)
	defer stop()

	bridge := newFakeTunnelBridge()
	mgr := newTunnelMgr(bridge)
	defer mgr.shutdown()

	if _, err := mgr.open("tn1", port, "127.0.0.1"); err != nil {
		t.Fatalf("open: %v", err)
	}
	if err := mgr.openStream(context.Background(), "tn1", "s1"); err != nil {
		t.Fatalf("openStream: %v", err)
	}
	payload := []byte("hello tunnel\n")
	if err := mgr.writeData("tn1", "s1", payload); err != nil {
		t.Fatalf("writeData: %v", err)
	}

	select {
	case rec := <-bridge.dataCh:
		if rec.tunnelID != "tn1" || rec.streamID != "s1" {
			t.Fatalf("unexpected echo target tunnel=%s stream=%s", rec.tunnelID, rec.streamID)
		}
		if string(rec.payload) != string(payload) {
			t.Fatalf("echo payload = %q, want %q", rec.payload, payload)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("did not receive echoed bytes back from tunnel within 2s")
	}

	if ok := mgr.closeStream("tn1", "s1", "test cleanup"); !ok {
		t.Fatal("closeStream returned false")
	}
	if ok := mgr.closeTunnel("tn1", "test cleanup"); !ok {
		t.Fatal("closeTunnel returned false")
	}
}

func TestTunnelManagerWaitsForStreamBeforeWritingData(t *testing.T) {
	port, stop := startEchoServer(t)
	defer stop()

	bridge := newFakeTunnelBridge()
	mgr := newTunnelMgr(bridge)
	defer mgr.shutdown()

	if _, err := mgr.open("tn1", port, "127.0.0.1"); err != nil {
		t.Fatalf("open: %v", err)
	}

	payload := []byte("request raced stream open")
	writeDone := make(chan error, 1)
	go func() {
		writeDone <- mgr.writeData("tn1", "s1", payload)
	}()

	time.Sleep(50 * time.Millisecond)
	if err := mgr.openStream(context.Background(), "tn1", "s1"); err != nil {
		t.Fatalf("openStream: %v", err)
	}
	if err := <-writeDone; err != nil {
		t.Fatalf("writeData: %v", err)
	}

	select {
	case rec := <-bridge.dataCh:
		if string(rec.payload) != string(payload) {
			t.Fatalf("echo payload = %q, want %q", rec.payload, payload)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("did not receive echoed bytes back from tunnel within 2s")
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
	if tun.localHost != "192.168.2.1" {
		t.Fatalf("localHost = %q, want 192.168.2.1", tun.localHost)
	}
}

func TestTunnelManagerKeepsPrivateHostname(t *testing.T) {
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

func TestTunnelManagerDataRejectsOversized(t *testing.T) {
	port, stop := startEchoServer(t)
	defer stop()
	mgr := newTunnelMgr(newFakeTunnelBridge())
	defer mgr.shutdown()
	if _, err := mgr.open("tn1", port, "127.0.0.1"); err != nil {
		t.Fatalf("open: %v", err)
	}
	if err := mgr.openStream(context.Background(), "tn1", "s1"); err != nil {
		t.Fatalf("openStream: %v", err)
	}
	big := make([]byte, tunnelMaxFramePayload+1)
	if err := mgr.writeData("tn1", "s1", big); err == nil {
		t.Fatal("expected oversized payload to be rejected")
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

func TestTunnelStreamOpenDataAndCloseRoundTrip(t *testing.T) {
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
	if _, err := tunnelStreamOpenHandler(context.Background(), rawArgs(t, map[string]interface{}{
		"tunnel_id": "tn1",
		"stream_id": "s1",
	})); err != nil {
		t.Fatalf("tunnelStreamOpen: %v", err)
	}
	payload := []byte("round trip bytes")
	if _, err := tunnelDataHandler(context.Background(), rawArgs(t, map[string]interface{}{
		"tunnel_id":   "tn1",
		"stream_id":   "s1",
		"data_base64": base64.StdEncoding.EncodeToString(payload),
	})); err != nil {
		t.Fatalf("tunnelData: %v", err)
	}

	select {
	case rec := <-bridge.dataCh:
		if string(rec.payload) != string(payload) {
			t.Fatalf("payload = %q, want %q", rec.payload, payload)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("did not see echoed payload within 2s")
	}

	res, err := tunnelStreamCloseHandler(context.Background(), rawArgs(t, map[string]interface{}{
		"tunnel_id": "tn1",
		"stream_id": "s1",
	}))
	if err != nil {
		t.Fatalf("tunnelStreamClose: %v", err)
	}
	if got := res.(map[string]interface{})["closed"]; got != true {
		t.Fatalf("stream close result = %v, want closed=true", res)
	}
	res, err = tunnelCloseHandler(context.Background(), rawArgs(t, map[string]interface{}{
		"tunnel_id": "tn1",
	}))
	if err != nil {
		t.Fatalf("tunnelClose: %v", err)
	}
	if got := res.(map[string]interface{})["closed"]; got != true {
		t.Fatalf("tunnel close result = %v, want closed=true", res)
	}
}

func TestTunnelDataDecodesBase64(t *testing.T) {
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
	if _, err := tunnelStreamOpenHandler(context.Background(), rawArgs(t, map[string]interface{}{
		"tunnel_id": "tn1",
		"stream_id": "s1",
	})); err != nil {
		t.Fatalf("tunnelStreamOpen: %v", err)
	}
	if _, err := tunnelDataHandler(context.Background(), rawArgs(t, map[string]interface{}{
		"tunnel_id":   "tn1",
		"stream_id":   "s1",
		"data_base64": "$$$not base64!!!",
	})); err == nil {
		t.Fatal("expected bad base64 to fail")
	}
}

func TestIsLoopbackHostAcceptsLocalhostAndLoopback(t *testing.T) {
	for _, h := range []string{"localhost", "127.0.0.1", "::1"} {
		if !isLoopbackHost(h) {
			t.Fatalf("isLoopbackHost(%q) = false, want true", h)
		}
	}
	for _, h := range []string{"", "8.8.8.8", "192.168.1.1", "example.com"} {
		if isLoopbackHost(h) {
			t.Fatalf("isLoopbackHost(%q) = true, want false", h)
		}
	}
}

func TestTunnelManagerShutdownClosesActiveStreams(t *testing.T) {
	port, stop := startEchoServer(t)
	defer stop()
	mgr := newTunnelMgr(newFakeTunnelBridge())
	if _, err := mgr.open("tn1", port, "127.0.0.1"); err != nil {
		t.Fatalf("open: %v", err)
	}
	if err := mgr.openStream(context.Background(), "tn1", "s1"); err != nil {
		t.Fatalf("openStream: %v", err)
	}
	mgr.shutdown()
	// After shutdown the stream entry must be gone -- closeStream returns false.
	if ok := mgr.closeStream("tn1", "s1", "after-shutdown"); ok {
		t.Fatal("closeStream succeeded after shutdown")
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

func TestTunnelPortStringFormatting(t *testing.T) {
	if got := strconv.Itoa(65535); got != "65535" {
		t.Fatalf("Itoa = %q", got)
	}
}

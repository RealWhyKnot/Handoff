// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
	"github.com/RealWhyKnot/Handoff/internal/supportlog"
)

// Tunnel wire protocol (host bridge frames). The relay drives this:
//
//   relay -> host (Commands):
//     tunnel.open         { tunnel_id, local_port, host? }
//     tunnel.stream_open  { tunnel_id, stream_id }
//     tunnel.data         { tunnel_id, stream_id, data_base64 }
//     tunnel.stream_close { tunnel_id, stream_id, reason? }
//     tunnel.close        { tunnel_id, reason? }
//
//   host -> relay (TelemetryEvent kinds):
//     tunnel_data         { tunnel_id, stream_id, data_base64 }
//     tunnel_stream_close { tunnel_id, stream_id, reason? }
//     tunnel_close        { tunnel_id, reason? }
//
// tunnel.open is the consent-gated lifecycle command -- the host prompts the
// operator's PC owner once per session before any tunnel can open. The data /
// stream / close frames are fire-and-forget bytes through an already-trusted
// tunnel.
//
// Hosts only forward bytes to loopback (127.0.0.1 / ::1). The local_port is
// validated 1..65535 before anything is dialed.

// TunnelBridge is the subset of the relay bridge the tunnel manager needs.
// Defined locally to keep the capabilities package decoupled from internal/relay.
type TunnelBridge interface {
	SendTunnelData(ctx context.Context, tunnelID, streamID string, data []byte) error
	SendTunnelStreamClose(ctx context.Context, tunnelID, streamID, reason string) error
	SendTunnelClose(ctx context.Context, tunnelID, reason string) error
}

const (
	tunnelReadChunk      = 16 * 1024
	tunnelMaxFramePayload = 1 * 1024 * 1024 // operator-side write cap per frame
	tunnelDialTimeout    = 5 * time.Second
)

var (
	tunnelMu      sync.Mutex
	tunnelManager *tunnelMgr
)

// RegisterTunnel wires the tunnel.* command kinds. The bridge is captured so
// the read pump can ship bytes back without going through the dispatcher.
func RegisterTunnel(r *dispatch.Router, bridge TunnelBridge) {
	tunnelMu.Lock()
	if tunnelManager != nil {
		tunnelManager.shutdown()
	}
	tunnelManager = newTunnelMgr(bridge)
	tunnelMu.Unlock()

	r.Register("tunnel.open", tunnelOpenHandler)
	r.Register("tunnel.stream_open", tunnelStreamOpenHandler)
	r.Register("tunnel.data", tunnelDataHandler)
	r.Register("tunnel.stream_close", tunnelStreamCloseHandler)
	r.Register("tunnel.close", tunnelCloseHandler)
}

// activeTunnelManager returns the currently registered manager. Used by tests.
func activeTunnelManager() *tunnelMgr {
	tunnelMu.Lock()
	defer tunnelMu.Unlock()
	return tunnelManager
}

type tunnelMgr struct {
	bridge TunnelBridge

	mu      sync.Mutex
	tunnels map[string]*tunnelState
}

type tunnelState struct {
	id        string
	localPort int
	localHost string

	mu      sync.Mutex
	streams map[string]*tunnelStream
	closed  bool
}

type tunnelStream struct {
	id     string
	conn   net.Conn
	tun    *tunnelState
	cancel context.CancelFunc
}

func newTunnelMgr(bridge TunnelBridge) *tunnelMgr {
	return &tunnelMgr{
		bridge:  bridge,
		tunnels: map[string]*tunnelState{},
	}
}

func (m *tunnelMgr) shutdown() {
	m.mu.Lock()
	states := make([]*tunnelState, 0, len(m.tunnels))
	for _, t := range m.tunnels {
		states = append(states, t)
	}
	m.tunnels = map[string]*tunnelState{}
	m.mu.Unlock()
	for _, t := range states {
		t.closeAll("host shutdown")
	}
}

func (m *tunnelMgr) open(tunnelID string, port int, host string) (*tunnelState, error) {
	if tunnelID == "" {
		return nil, errors.New("tunnel.open: tunnel_id is required")
	}
	if port <= 0 || port > 65535 {
		return nil, fmt.Errorf("tunnel.open: local_port must be 1..65535 (got %d)", port)
	}
	host = strings.TrimSpace(host)
	if host == "" {
		host = "127.0.0.1"
	}
	if !isLoopbackHost(host) {
		return nil, fmt.Errorf("tunnel.open: only loopback hosts allowed (got %q)", host)
	}

	m.mu.Lock()
	defer m.mu.Unlock()
	if existing, ok := m.tunnels[tunnelID]; ok {
		if existing.localPort != port || existing.localHost != host {
			return nil, fmt.Errorf("tunnel.open: tunnel_id %s already open on a different port", tunnelID)
		}
		return existing, nil
	}
	t := &tunnelState{
		id:        tunnelID,
		localPort: port,
		localHost: host,
		streams:   map[string]*tunnelStream{},
	}
	m.tunnels[tunnelID] = t
	return t, nil
}

func (m *tunnelMgr) get(tunnelID string) (*tunnelState, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	t, ok := m.tunnels[tunnelID]
	return t, ok
}

func (m *tunnelMgr) closeTunnel(tunnelID, reason string) bool {
	m.mu.Lock()
	t, ok := m.tunnels[tunnelID]
	if ok {
		delete(m.tunnels, tunnelID)
	}
	m.mu.Unlock()
	if !ok {
		return false
	}
	t.closeAll(reason)
	return true
}

func (m *tunnelMgr) openStream(ctx context.Context, tunnelID, streamID string) error {
	t, ok := m.get(tunnelID)
	if !ok {
		return fmt.Errorf("tunnel.stream_open: unknown tunnel_id %s", tunnelID)
	}
	if streamID == "" {
		return errors.New("tunnel.stream_open: stream_id is required")
	}

	t.mu.Lock()
	if t.closed {
		t.mu.Unlock()
		return fmt.Errorf("tunnel.stream_open: tunnel %s is closed", tunnelID)
	}
	if _, exists := t.streams[streamID]; exists {
		t.mu.Unlock()
		return fmt.Errorf("tunnel.stream_open: stream_id %s already open", streamID)
	}
	t.mu.Unlock()

	dialCtx, cancel := context.WithTimeout(ctx, tunnelDialTimeout)
	defer cancel()
	dialer := &net.Dialer{}
	conn, err := dialer.DialContext(dialCtx, "tcp", net.JoinHostPort(t.localHost, strconv.Itoa(t.localPort)))
	if err != nil {
		return fmt.Errorf("tunnel.stream_open: dial %s:%d: %w", t.localHost, t.localPort, err)
	}

	pumpCtx, pumpCancel := context.WithCancel(context.Background())
	stream := &tunnelStream{id: streamID, conn: conn, tun: t, cancel: pumpCancel}

	t.mu.Lock()
	if t.closed {
		t.mu.Unlock()
		pumpCancel()
		_ = conn.Close()
		return fmt.Errorf("tunnel.stream_open: tunnel %s closed during dial", tunnelID)
	}
	t.streams[streamID] = stream
	t.mu.Unlock()

	go m.readPump(pumpCtx, t.id, stream)
	supportlog.Printf("tunnel stream open tunnel=%s stream=%s local=%s:%d", t.id, streamID, t.localHost, t.localPort)
	return nil
}

func (m *tunnelMgr) writeData(tunnelID, streamID string, payload []byte) error {
	t, ok := m.get(tunnelID)
	if !ok {
		return fmt.Errorf("tunnel.data: unknown tunnel_id %s", tunnelID)
	}
	t.mu.Lock()
	s, ok := t.streams[streamID]
	t.mu.Unlock()
	if !ok {
		return fmt.Errorf("tunnel.data: unknown stream_id %s", streamID)
	}
	if len(payload) > tunnelMaxFramePayload {
		return fmt.Errorf("tunnel.data: payload %d bytes exceeds cap %d", len(payload), tunnelMaxFramePayload)
	}
	if _, err := s.conn.Write(payload); err != nil {
		s.close("write error: " + err.Error())
		return fmt.Errorf("tunnel.data: write: %w", err)
	}
	return nil
}

func (m *tunnelMgr) closeStream(tunnelID, streamID, reason string) bool {
	t, ok := m.get(tunnelID)
	if !ok {
		return false
	}
	t.mu.Lock()
	s, ok := t.streams[streamID]
	if ok {
		delete(t.streams, streamID)
	}
	t.mu.Unlock()
	if !ok {
		return false
	}
	s.close(reason)
	return true
}

func (m *tunnelMgr) readPump(ctx context.Context, tunnelID string, s *tunnelStream) {
	buf := make([]byte, tunnelReadChunk)
	defer func() {
		s.tun.mu.Lock()
		if existing, ok := s.tun.streams[s.id]; ok && existing == s {
			delete(s.tun.streams, s.id)
		}
		s.tun.mu.Unlock()
		_ = s.conn.Close()
	}()
	for {
		if ctx.Err() != nil {
			return
		}
		n, err := s.conn.Read(buf)
		if n > 0 {
			chunk := make([]byte, n)
			copy(chunk, buf[:n])
			if sendErr := m.bridge.SendTunnelData(ctx, tunnelID, s.id, chunk); sendErr != nil {
				supportlog.Printf("tunnel send data failed tunnel=%s stream=%s: %v", tunnelID, s.id, sendErr)
				return
			}
		}
		if err != nil {
			reason := "eof"
			if !errors.Is(err, io.EOF) {
				reason = "read error: " + err.Error()
			}
			_ = m.bridge.SendTunnelStreamClose(ctx, tunnelID, s.id, reason)
			supportlog.Printf("tunnel stream end tunnel=%s stream=%s reason=%s", tunnelID, s.id, reason)
			return
		}
	}
}

func (s *tunnelStream) close(reason string) {
	if s.cancel != nil {
		s.cancel()
	}
	_ = s.conn.Close()
	supportlog.Printf("tunnel stream close tunnel=%s stream=%s reason=%s", s.tun.id, s.id, reason)
}

func (t *tunnelState) closeAll(reason string) {
	t.mu.Lock()
	t.closed = true
	streams := make([]*tunnelStream, 0, len(t.streams))
	for _, s := range t.streams {
		streams = append(streams, s)
	}
	t.streams = map[string]*tunnelStream{}
	t.mu.Unlock()
	for _, s := range streams {
		s.close(reason)
	}
	supportlog.Printf("tunnel close tunnel=%s reason=%s", t.id, reason)
}

func isLoopbackHost(host string) bool {
	if host == "localhost" {
		return true
	}
	ip := net.ParseIP(host)
	return ip != nil && ip.IsLoopback()
}

// ---- handlers ----

func tunnelOpenHandler(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var tunnelID string
	var hostArg string
	port := 0
	if v, ok := args["tunnel_id"]; ok {
		_ = json.Unmarshal(v, &tunnelID)
	}
	if v, ok := args["local_port"]; ok {
		_ = json.Unmarshal(v, &port)
	}
	if v, ok := args["host"]; ok {
		_ = json.Unmarshal(v, &hostArg)
	}

	mgr := activeTunnelManager()
	if mgr == nil {
		return nil, errors.New("tunnel.open: tunnel manager not registered")
	}

	summary := fmt.Sprintf("Opens a tunnel from the operator to %s:%d on this computer. The operator will be able to reach that local service for as long as the tunnel stays open.",
		hostOrDefault(hostArg), port)
	if err := requireRiskConsent(ctx, "tunnel.open", summary); err != nil {
		return nil, err
	}
	t, err := mgr.open(tunnelID, port, hostArg)
	if err != nil {
		return nil, err
	}
	supportlog.Printf("tunnel open tunnel=%s local=%s:%d", t.id, t.localHost, t.localPort)
	return map[string]interface{}{
		"tunnel_id":  t.id,
		"local_port": t.localPort,
		"local_host": t.localHost,
	}, nil
}

func tunnelStreamOpenHandler(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var tunnelID, streamID string
	if v, ok := args["tunnel_id"]; ok {
		_ = json.Unmarshal(v, &tunnelID)
	}
	if v, ok := args["stream_id"]; ok {
		_ = json.Unmarshal(v, &streamID)
	}
	mgr := activeTunnelManager()
	if mgr == nil {
		return nil, errors.New("tunnel.stream_open: tunnel manager not registered")
	}
	if err := mgr.openStream(ctx, tunnelID, streamID); err != nil {
		return nil, err
	}
	return map[string]interface{}{"opened": true}, nil
}

func tunnelDataHandler(_ context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var tunnelID, streamID, dataB64 string
	if v, ok := args["tunnel_id"]; ok {
		_ = json.Unmarshal(v, &tunnelID)
	}
	if v, ok := args["stream_id"]; ok {
		_ = json.Unmarshal(v, &streamID)
	}
	if v, ok := args["data_base64"]; ok {
		_ = json.Unmarshal(v, &dataB64)
	}
	mgr := activeTunnelManager()
	if mgr == nil {
		return nil, errors.New("tunnel.data: tunnel manager not registered")
	}
	payload, err := base64.StdEncoding.DecodeString(dataB64)
	if err != nil {
		return nil, fmt.Errorf("tunnel.data: data_base64: %w", err)
	}
	if err := mgr.writeData(tunnelID, streamID, payload); err != nil {
		return nil, err
	}
	return map[string]interface{}{"bytes": len(payload)}, nil
}

func tunnelStreamCloseHandler(_ context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var tunnelID, streamID, reason string
	if v, ok := args["tunnel_id"]; ok {
		_ = json.Unmarshal(v, &tunnelID)
	}
	if v, ok := args["stream_id"]; ok {
		_ = json.Unmarshal(v, &streamID)
	}
	if v, ok := args["reason"]; ok {
		_ = json.Unmarshal(v, &reason)
	}
	mgr := activeTunnelManager()
	if mgr == nil {
		return map[string]interface{}{"closed": false}, nil
	}
	if reason == "" {
		reason = "operator close"
	}
	return map[string]interface{}{"closed": mgr.closeStream(tunnelID, streamID, reason)}, nil
}

func tunnelCloseHandler(_ context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var tunnelID, reason string
	if v, ok := args["tunnel_id"]; ok {
		_ = json.Unmarshal(v, &tunnelID)
	}
	if v, ok := args["reason"]; ok {
		_ = json.Unmarshal(v, &reason)
	}
	mgr := activeTunnelManager()
	if mgr == nil {
		return map[string]interface{}{"closed": false}, nil
	}
	if reason == "" {
		reason = "operator close"
	}
	return map[string]interface{}{"closed": mgr.closeTunnel(tunnelID, reason)}, nil
}

func hostOrDefault(host string) string {
	host = strings.TrimSpace(host)
	if host == "" {
		return "127.0.0.1"
	}
	return host
}

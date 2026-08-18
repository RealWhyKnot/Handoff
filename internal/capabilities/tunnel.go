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
// Hosts only forward bytes to loopback or local network destinations. Hostnames
// are resolved before the tunnel is opened and must resolve only to loopback,
// private, or link-local addresses. The original hostname is kept for dialing so
// router/repeater admin pages that key off the host name still see the expected
// target.

// TunnelBridge is the subset of the relay bridge the tunnel manager needs.
// Defined locally to keep the capabilities package decoupled from internal/relay.
type TunnelBridge interface {
	SendTunnelData(ctx context.Context, tunnelID, streamID string, data []byte) error
	SendTunnelStreamClose(ctx context.Context, tunnelID, streamID, reason string) error
	SendTunnelClose(ctx context.Context, tunnelID, reason string) error
}

const (
	tunnelReadChunk        = 16 * 1024
	tunnelMaxFramePayload  = 1 * 1024 * 1024 // operator-side write cap per frame
	tunnelDialTimeout      = 5 * time.Second
	tunnelHostLookupTime   = 3 * time.Second
	tunnelStreamQueueDepth = 1024             // buffered frames per stream before the receive loop backpressures
	tunnelWriteDeadline    = 60 * time.Second // a wedged local peer can't block a stream forever
)

var (
	tunnelMu      sync.Mutex
	tunnelManager *tunnelMgr

	tunnelLookupIP = func(ctx context.Context, host string) ([]net.IPAddr, error) {
		return net.DefaultResolver.LookupIPAddr(ctx, host)
	}
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
	r.Register("tunnel.close", tunnelCloseHandler)
}

// tunnelFrameKinds are the per-stream data frames the host receive loop routes
// to the ordered consumer instead of the goroutine-per-command dispatcher, so
// bytes for one stream reach the socket in wire order.
var tunnelFrameKinds = map[string]bool{
	"tunnel.stream_open":  true,
	"tunnel.data":         true,
	"tunnel.stream_close": true,
}

// IsTunnelFrameKind reports whether a command kind is an ordered tunnel data
// frame that must bypass the dispatcher.
func IsTunnelFrameKind(kind string) bool { return tunnelFrameKinds[kind] }

// EnqueueTunnelFrame hands a tunnel data frame to its stream's ordered queue.
// Safe to call with any args; unknown tunnels/streams are dropped.
func EnqueueTunnelFrame(kind string, args map[string]json.RawMessage) {
	mgr := activeTunnelManager()
	if mgr == nil {
		return
	}
	mgr.enqueueFrame(kind, rawString(args, "tunnel_id"), rawString(args, "stream_id"),
		rawString(args, "data_base64"), rawString(args, "reason"))
}

func rawString(args map[string]json.RawMessage, key string) string {
	v, ok := args[key]
	if !ok {
		return ""
	}
	var s string
	_ = json.Unmarshal(v, &s)
	return s
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
	localHost string // display name (hostname or IP) for logs and the consent prompt
	dialHost  string // pinned IP validated at open time; dialed instead of re-resolving

	mgr    *tunnelMgr
	done   chan struct{}
	doneMu sync.Once

	mu      sync.Mutex
	streams map[string]*tunnelStream
	queues  map[string]chan tunnelFrame
	closed  bool
}

type tunnelStream struct {
	id     string
	conn   net.Conn
	tun    *tunnelState
	cancel context.CancelFunc
}

type frameKind int

const (
	frameStreamOpen frameKind = iota
	frameData
	frameStreamClose
)

type tunnelFrame struct {
	kind    frameKind
	payload []byte
	reason  string
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
	displayHost, dialIP, err := validateTunnelTargetHost(host)
	if err != nil {
		return nil, err
	}

	m.mu.Lock()
	defer m.mu.Unlock()
	if existing, ok := m.tunnels[tunnelID]; ok {
		if existing.localPort != port || existing.localHost != displayHost {
			return nil, fmt.Errorf("tunnel.open: tunnel_id %s already open on a different port", tunnelID)
		}
		return existing, nil
	}
	t := &tunnelState{
		id:        tunnelID,
		localPort: port,
		localHost: displayHost,
		dialHost:  dialIP,
		mgr:       m,
		done:      make(chan struct{}),
		streams:   map[string]*tunnelStream{},
		queues:    map[string]chan tunnelFrame{},
	}
	m.tunnels[tunnelID] = t
	return t, nil
}

func (t *tunnelState) dialTarget() string {
	return net.JoinHostPort(t.dialHost, strconv.Itoa(t.localPort))
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

// enqueueFrame routes a wire frame to the owning stream's ordered queue. It is
// called from the host receive loop in wire order, so a single consumer goroutine
// per stream preserves byte order without racing goroutines -- the correctness
// guarantee HTTP traffic depends on.
func (m *tunnelMgr) enqueueFrame(kind, tunnelID, streamID, dataB64, reason string) {
	t, ok := m.get(tunnelID)
	if !ok || streamID == "" {
		return
	}
	switch kind {
	case "tunnel.stream_open":
		t.enqueue(streamID, tunnelFrame{kind: frameStreamOpen})
	case "tunnel.data":
		payload, err := base64.StdEncoding.DecodeString(dataB64)
		if err != nil {
			supportlog.Printf("tunnel data bad base64 tunnel=%s stream=%s: %v", tunnelID, streamID, err)
			return
		}
		if len(payload) > tunnelMaxFramePayload {
			supportlog.Printf("tunnel data oversize tunnel=%s stream=%s bytes=%d", tunnelID, streamID, len(payload))
			_ = m.bridge.SendTunnelStreamClose(context.Background(), tunnelID, streamID, "frame too large")
			t.enqueue(streamID, tunnelFrame{kind: frameStreamClose, reason: "frame too large"})
			return
		}
		t.enqueue(streamID, tunnelFrame{kind: frameData, payload: payload})
	case "tunnel.stream_close":
		if reason == "" {
			reason = "operator close"
		}
		t.enqueue(streamID, tunnelFrame{kind: frameStreamClose, reason: reason})
	}
}

func (t *tunnelState) enqueue(streamID string, f tunnelFrame) {
	t.mu.Lock()
	if t.closed {
		t.mu.Unlock()
		return
	}
	ch, ok := t.queues[streamID]
	if !ok {
		ch = make(chan tunnelFrame, tunnelStreamQueueDepth)
		t.queues[streamID] = ch
		go t.consume(streamID, ch)
	}
	t.mu.Unlock()
	select {
	case ch <- f:
	case <-t.done:
	}
}

// consume drains one stream's frames in order: dial on stream_open, write data in
// arrival order, tear down on stream_close or tunnel close.
func (t *tunnelState) consume(streamID string, ch chan tunnelFrame) {
	defer func() {
		if r := recover(); r != nil {
			supportlog.Printf("tunnel consumer panic tunnel=%s stream=%s: %v", t.id, streamID, r)
		}
		t.removeQueue(streamID)
	}()
	var s *tunnelStream
	for {
		select {
		case <-t.done:
			if s != nil {
				s.close("tunnel closed")
			}
			return
		case f := <-ch:
			switch f.kind {
			case frameStreamOpen:
				if s != nil {
					continue
				}
				st, err := t.mgr.dialStream(t, streamID)
				if err != nil {
					supportlog.Printf("tunnel dial failed tunnel=%s stream=%s: %v", t.id, streamID, err)
					_ = t.mgr.bridge.SendTunnelStreamClose(context.Background(), t.id, streamID, "dial error: "+err.Error())
					continue
				}
				s = st
			case frameData:
				if s == nil {
					continue
				}
				if err := s.write(f.payload); err != nil {
					supportlog.Printf("tunnel write failed tunnel=%s stream=%s: %v", t.id, streamID, err)
					_ = t.mgr.bridge.SendTunnelStreamClose(context.Background(), t.id, streamID, "write error: "+err.Error())
					s.close("write error")
					s = nil
				}
			case frameStreamClose:
				if s != nil {
					s.close(f.reason)
				}
				return
			}
		}
	}
}

func (t *tunnelState) removeQueue(streamID string) {
	t.mu.Lock()
	delete(t.queues, streamID)
	t.mu.Unlock()
}

func (m *tunnelMgr) dialStream(t *tunnelState, streamID string) (*tunnelStream, error) {
	t.mu.Lock()
	if t.closed {
		t.mu.Unlock()
		return nil, fmt.Errorf("tunnel %s is closed", t.id)
	}
	if _, exists := t.streams[streamID]; exists {
		t.mu.Unlock()
		return nil, fmt.Errorf("stream_id %s already open", streamID)
	}
	target := t.dialTarget()
	t.mu.Unlock()

	dialCtx, cancel := context.WithTimeout(context.Background(), tunnelDialTimeout)
	defer cancel()
	conn, err := (&net.Dialer{}).DialContext(dialCtx, "tcp", target)
	if err != nil {
		return nil, fmt.Errorf("dial %s: %w", target, err)
	}

	pumpCtx, pumpCancel := context.WithCancel(context.Background())
	stream := &tunnelStream{id: streamID, conn: conn, tun: t, cancel: pumpCancel}

	t.mu.Lock()
	if t.closed {
		t.mu.Unlock()
		pumpCancel()
		_ = conn.Close()
		return nil, fmt.Errorf("tunnel %s closed during dial", t.id)
	}
	t.streams[streamID] = stream
	t.mu.Unlock()

	go m.readPump(pumpCtx, t.id, stream)
	supportlog.Printf("tunnel stream open tunnel=%s stream=%s local=%s", t.id, streamID, target)
	return stream, nil
}

func (s *tunnelStream) write(payload []byte) error {
	_ = s.conn.SetWriteDeadline(time.Now().Add(tunnelWriteDeadline))
	_, err := s.conn.Write(payload)
	return err
}

func (m *tunnelMgr) readPump(ctx context.Context, tunnelID string, s *tunnelStream) {
	defer func() {
		if r := recover(); r != nil {
			supportlog.Printf("tunnel readpump panic tunnel=%s stream=%s: %v", tunnelID, s.id, r)
		}
	}()
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
	t.doneMu.Do(func() { close(t.done) })
	for _, s := range streams {
		s.close(reason)
	}
	supportlog.Printf("tunnel close tunnel=%s reason=%s", t.id, reason)
}

// validateTunnelTargetHost returns the display host (hostname or IP for logs and
// the consent prompt) and the pinned IP to dial. Pinning closes the TOCTOU gap:
// the address is validated once here and dialed as-is, never re-resolved.
func validateTunnelTargetHost(host string) (display, dialIP string, err error) {
	host = strings.TrimSpace(host)
	if host == "" {
		return "127.0.0.1", "127.0.0.1", nil
	}
	if strings.Contains(host, "://") || strings.ContainsAny(host, "/\\?#@") {
		return "", "", fmt.Errorf("tunnel.open: host must be a hostname or IP address (got %q)", host)
	}
	if _, _, splitErr := net.SplitHostPort(host); splitErr == nil {
		return "", "", fmt.Errorf("tunnel.open: host must not include a port (got %q)", host)
	}
	if ip := net.ParseIP(host); ip != nil {
		if !isAllowedTunnelTargetIP(ip) {
			return "", "", fmt.Errorf("tunnel.open: host %q is not a local network address", host)
		}
		return host, host, nil
	}
	if !isReasonableTunnelHostname(host) {
		return "", "", fmt.Errorf("tunnel.open: invalid host %q", host)
	}

	lookupCtx, cancel := context.WithTimeout(context.Background(), tunnelHostLookupTime)
	defer cancel()
	addrs, lookupErr := tunnelLookupIP(lookupCtx, host)
	if lookupErr != nil {
		return "", "", fmt.Errorf("tunnel.open: resolve host %q: %w", host, lookupErr)
	}
	if len(addrs) == 0 {
		return "", "", fmt.Errorf("tunnel.open: resolve host %q: no addresses", host)
	}
	for _, addr := range addrs {
		if !isAllowedTunnelTargetIP(addr.IP) {
			return "", "", fmt.Errorf("tunnel.open: host %q resolves outside the local network (%s)", host, addr.IP.String())
		}
	}
	return host, addrs[0].IP.String(), nil
}

func isReasonableTunnelHostname(host string) bool {
	if len(host) > 253 {
		return false
	}
	for _, r := range host {
		if r <= 0x20 || r > 0x7e {
			return false
		}
		if !(r == '.' || r == '-' || r == '_' || r >= '0' && r <= '9' || r >= 'A' && r <= 'Z' || r >= 'a' && r <= 'z') {
			return false
		}
	}
	return true
}

func isAllowedTunnelTargetIP(ip net.IP) bool {
	if ip == nil || ip.IsUnspecified() || ip.IsMulticast() || ip.IsInterfaceLocalMulticast() {
		return false
	}
	if ip.IsLoopback() || ip.IsPrivate() || ip.IsLinkLocalUnicast() {
		return true
	}
	// 100.64.0.0/10 CGNAT -- some ISP-managed router pages live here.
	if v4 := ip.To4(); v4 != nil && v4[0] == 100 && v4[1]&0xc0 == 64 {
		return true
	}
	return false
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

	summary := fmt.Sprintf("Opens a network tunnel so the helper can reach %s. It stays open until you end the session or the helper closes it.",
		tunnelTargetDescription(hostArg, port))
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

// tunnelTargetDescription is the plain-language phrase shown in the consent
// prompt, honest about whether the target is on this PC or elsewhere on the LAN.
func tunnelTargetDescription(hostArg string, port int) string {
	host := strings.TrimSpace(hostArg)
	if host == "" {
		host = "127.0.0.1"
	}
	if strings.EqualFold(host, "localhost") {
		return fmt.Sprintf("a service running on this computer (%s:%d)", host, port)
	}
	if ip := net.ParseIP(host); ip != nil && ip.IsLoopback() {
		return fmt.Sprintf("a service running on this computer (%s:%d)", host, port)
	}
	return fmt.Sprintf("a device on this computer's local network -- %s:%d (for example a router or printer)", host, port)
}

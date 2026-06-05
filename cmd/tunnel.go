// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/supportlog"
	"github.com/RealWhyKnot/Handoff/internal/visibility"
	"github.com/coder/websocket"
)

// Tunnel implements the operator-side `handoff tunnel` client. The website
// (after the host approves tunnel.open) shows a connect token; the operator
// types it into a handoff prompt, picks a local port, and this code binds
// 127.0.0.1:<localPort> while forwarding every accepted TCP connection through
// the relay's tunnel WebSocket to the host.
//
// Operator <-> relay frames (text JSON):
//
//	{kind:"tunnel_ready",  tunnel_id, host_port}
//	{kind:"stream_open",   stream_id}                     -- operator -> relay
//	{kind:"data",          stream_id, data_base64}        -- bidirectional
//	{kind:"stream_close",  stream_id, reason}             -- bidirectional
//	{kind:"tunnel_closed", reason}                        -- relay -> operator
//	{kind:"error",         message}                       -- relay -> operator
func Tunnel(args []string) {
	opts, err := parseTunnelArgs(args)
	if err != nil {
		fmt.Fprintln(os.Stderr, "handoff tunnel:", err)
		fmt.Fprintln(os.Stderr, "usage: handoff tunnel <connect-token> [--local-port PORT] [--relay URL]")
		os.Exit(2)
	}

	supportlog.Printf("tunnel-client start token=%s local=%d relay=%s",
		shortSid(opts.token), opts.localPort, opts.relay)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt, syscall.SIGTERM)
	go func() {
		<-sig
		fmt.Println("\nclosing tunnel...")
		supportlog.Printf("tunnel-client shutdown requested")
		cancel()
	}()

	wsURL, err := tunnelWsURL(opts.relay, opts.token)
	if err != nil {
		fmt.Fprintln(os.Stderr, "handoff tunnel:", err)
		os.Exit(1)
	}
	fmt.Println("connecting to relay:", opts.relay)

	dialCtx, dialCancel := context.WithTimeout(ctx, 15*time.Second)
	conn, _, err := websocket.Dial(dialCtx, wsURL, &websocket.DialOptions{HTTPHeader: http.Header{}})
	dialCancel()
	if err != nil {
		fmt.Fprintln(os.Stderr, "could not open tunnel:", err)
		os.Exit(1)
	}
	defer conn.Close(websocket.StatusNormalClosure, "bye")
	conn.SetReadLimit(8 << 20)

	// Expect a tunnel_ready (or error) before binding the local listener.
	ready, err := awaitTunnelReady(ctx, conn)
	if err != nil {
		fmt.Fprintln(os.Stderr, "tunnel handshake failed:", err)
		os.Exit(1)
	}
	fmt.Printf("tunnel ready -- forwarding 127.0.0.1:%d -> host %s:%d\n",
		opts.localPort, ready.HostAddr, ready.HostPort)

	listener, err := net.Listen("tcp", net.JoinHostPort("127.0.0.1", strconv.Itoa(opts.localPort)))
	if err != nil {
		fmt.Fprintln(os.Stderr, "could not bind local port:", err)
		os.Exit(1)
	}
	defer listener.Close()
	fmt.Println("press Ctrl+C to close the tunnel.")

	client := newTunnelClient(conn)
	defer client.shutdown("operator close")

	// Lifecycle hook for foreground-window policy. Currently a no-op.
	visibility.StartWatcher(ctx)

	go client.acceptLoop(ctx, listener)
	if err := client.readLoop(ctx); err != nil && !errors.Is(err, context.Canceled) {
		supportlog.Printf("tunnel client read loop ended: %v", err)
	}
}

type tunnelOptions struct {
	token     string
	localPort int
	relay     string
}

func parseTunnelArgs(args []string) (tunnelOptions, error) {
	opts := tunnelOptions{localPort: 0, relay: defaultRelay()}
	positional := 0
	for i := 0; i < len(args); i++ {
		a := args[i]
		switch {
		case a == "--local-port" || a == "-p":
			if i+1 >= len(args) {
				return opts, errors.New("--local-port requires a value")
			}
			i++
			port, err := strconv.Atoi(args[i])
			if err != nil || port <= 0 || port > 65535 {
				return opts, fmt.Errorf("invalid --local-port %q", args[i])
			}
			opts.localPort = port
		case a == "--relay":
			if i+1 >= len(args) {
				return opts, errors.New("--relay requires a value")
			}
			i++
			opts.relay = args[i]
		case strings.HasPrefix(a, "--local-port="):
			port, err := strconv.Atoi(strings.TrimPrefix(a, "--local-port="))
			if err != nil || port <= 0 || port > 65535 {
				return opts, fmt.Errorf("invalid --local-port %q", a)
			}
			opts.localPort = port
		case strings.HasPrefix(a, "--relay="):
			opts.relay = strings.TrimPrefix(a, "--relay=")
		case strings.HasPrefix(a, "-"):
			return opts, fmt.Errorf("unknown flag %q", a)
		default:
			if positional == 0 {
				opts.token = a
				positional++
			} else {
				return opts, fmt.Errorf("unexpected argument %q", a)
			}
		}
	}
	opts.token = strings.TrimSpace(opts.token)
	if opts.token == "" {
		return opts, errors.New("connect token is required")
	}
	if opts.localPort == 0 {
		// Default to a stable mid-range local port; the operator can override.
		opts.localPort = 47800
	}
	return opts, nil
}

func tunnelWsURL(relayBase, token string) (string, error) {
	if isTunnelURL(token) {
		u, err := url.Parse(token)
		if err != nil {
			return "", err
		}
		return swapScheme(u).String(), nil
	}
	base := strings.TrimRight(relayBase, "/")
	u, err := url.Parse(base + "/api/tunnel/" + token)
	if err != nil {
		return "", err
	}
	return swapScheme(u).String(), nil
}

func isTunnelURL(token string) bool {
	return strings.HasPrefix(token, "http://") ||
		strings.HasPrefix(token, "https://") ||
		strings.HasPrefix(token, "ws://") ||
		strings.HasPrefix(token, "wss://")
}

func swapScheme(u *url.URL) *url.URL {
	switch u.Scheme {
	case "http":
		u.Scheme = "ws"
	case "https":
		u.Scheme = "wss"
	}
	return u
}

type tunnelReady struct {
	TunnelID string `json:"tunnel_id"`
	HostAddr string `json:"host_addr"`
	HostPort int    `json:"host_port"`
}

type tunnelFrame struct {
	Kind     string          `json:"kind"`
	StreamID string          `json:"stream_id,omitempty"`
	TunnelID string          `json:"tunnel_id,omitempty"`
	DataB64  string          `json:"data_base64,omitempty"`
	Reason   string          `json:"reason,omitempty"`
	Message  string          `json:"message,omitempty"`
	HostAddr string          `json:"host_addr,omitempty"`
	HostPort int             `json:"host_port,omitempty"`
	Extras   json.RawMessage `json:"-"`
}

func awaitTunnelReady(ctx context.Context, conn *websocket.Conn) (*tunnelReady, error) {
	readyCtx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	for {
		typ, data, err := conn.Read(readyCtx)
		if err != nil {
			return nil, err
		}
		if typ != websocket.MessageText {
			continue
		}
		var frame tunnelFrame
		if err := json.Unmarshal(data, &frame); err != nil {
			continue
		}
		switch frame.Kind {
		case "tunnel_ready":
			host := frame.HostAddr
			if host == "" {
				host = "127.0.0.1"
			}
			return &tunnelReady{TunnelID: frame.TunnelID, HostAddr: host, HostPort: frame.HostPort}, nil
		case "tunnel_closed":
			return nil, fmt.Errorf("relay closed tunnel: %s", frame.Reason)
		case "error":
			return nil, fmt.Errorf("relay refused tunnel: %s", frame.Message)
		}
	}
}

type tunnelClient struct {
	conn *websocket.Conn

	writeMu sync.Mutex

	mu      sync.Mutex
	streams map[string]net.Conn
	closed  bool
}

func newTunnelClient(conn *websocket.Conn) *tunnelClient {
	return &tunnelClient{conn: conn, streams: map[string]net.Conn{}}
}

func (t *tunnelClient) acceptLoop(ctx context.Context, listener net.Listener) {
	for {
		if ctx.Err() != nil {
			return
		}
		conn, err := listener.Accept()
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				return
			}
			supportlog.Printf("tunnel-client accept error: %v", err)
			return
		}
		streamID := newStreamID()
		t.register(streamID, conn)
		if err := t.sendFrame(ctx, tunnelFrame{Kind: "stream_open", StreamID: streamID}); err != nil {
			t.dropStream(streamID, "stream open send failed")
			continue
		}
		go t.copyToRelay(ctx, streamID, conn)
	}
}

func (t *tunnelClient) readLoop(ctx context.Context) error {
	for {
		typ, data, err := t.conn.Read(ctx)
		if err != nil {
			return err
		}
		if typ != websocket.MessageText {
			continue
		}
		var frame tunnelFrame
		if err := json.Unmarshal(data, &frame); err != nil {
			continue
		}
		switch frame.Kind {
		case "data":
			payload, derr := base64.StdEncoding.DecodeString(frame.DataB64)
			if derr != nil {
				supportlog.Printf("tunnel-client decode data error: %v", derr)
				continue
			}
			t.writeToStream(frame.StreamID, payload)
		case "stream_close":
			t.dropStream(frame.StreamID, frame.Reason)
		case "tunnel_closed":
			fmt.Println("relay closed tunnel:", frame.Reason)
			return io.EOF
		case "error":
			fmt.Fprintln(os.Stderr, "relay error:", frame.Message)
		}
	}
}

func (t *tunnelClient) copyToRelay(ctx context.Context, streamID string, conn net.Conn) {
	buf := make([]byte, 16*1024)
	defer t.dropStream(streamID, "local end of stream")
	for {
		if ctx.Err() != nil {
			return
		}
		n, err := conn.Read(buf)
		if n > 0 {
			chunk := make([]byte, n)
			copy(chunk, buf[:n])
			frame := tunnelFrame{
				Kind:     "data",
				StreamID: streamID,
				DataB64:  base64.StdEncoding.EncodeToString(chunk),
			}
			if sendErr := t.sendFrame(ctx, frame); sendErr != nil {
				return
			}
		}
		if err != nil {
			if !errors.Is(err, io.EOF) {
				supportlog.Printf("tunnel-client local read error stream=%s: %v", streamID, err)
			}
			_ = t.sendFrame(ctx, tunnelFrame{Kind: "stream_close", StreamID: streamID, Reason: "eof"})
			return
		}
	}
}

func (t *tunnelClient) sendFrame(ctx context.Context, frame tunnelFrame) error {
	data, err := json.Marshal(frame)
	if err != nil {
		return err
	}
	t.writeMu.Lock()
	defer t.writeMu.Unlock()
	if t.closed {
		return errors.New("tunnel client closed")
	}
	return t.conn.Write(ctx, websocket.MessageText, data)
}

func (t *tunnelClient) register(streamID string, conn net.Conn) {
	t.mu.Lock()
	t.streams[streamID] = conn
	t.mu.Unlock()
}

func (t *tunnelClient) writeToStream(streamID string, payload []byte) {
	t.mu.Lock()
	conn, ok := t.streams[streamID]
	t.mu.Unlock()
	if !ok {
		return
	}
	if _, err := conn.Write(payload); err != nil {
		t.dropStream(streamID, "write error: "+err.Error())
	}
}

func (t *tunnelClient) dropStream(streamID, reason string) {
	t.mu.Lock()
	conn, ok := t.streams[streamID]
	if ok {
		delete(t.streams, streamID)
	}
	t.mu.Unlock()
	if ok {
		_ = conn.Close()
		supportlog.Printf("tunnel-client stream end stream=%s reason=%s", streamID, reason)
	}
}

func (t *tunnelClient) shutdown(reason string) {
	t.writeMu.Lock()
	t.closed = true
	t.writeMu.Unlock()
	t.mu.Lock()
	streams := make([]net.Conn, 0, len(t.streams))
	for _, c := range t.streams {
		streams = append(streams, c)
	}
	t.streams = map[string]net.Conn{}
	t.mu.Unlock()
	for _, c := range streams {
		_ = c.Close()
	}
	_ = reason
}

func newStreamID() string {
	var b [8]byte
	if _, err := rand.Read(b[:]); err != nil {
		return fmt.Sprintf("s_%d", time.Now().UnixNano())
	}
	return "s_" + hex.EncodeToString(b[:])
}

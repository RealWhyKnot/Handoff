// SPDX-License-Identifier: GPL-3.0-or-later
//
// Package relay is the client-side of the host<->relay link. The HTTP side
// mints a fresh session; the WebSocket side runs the long-lived bridge that
// receives commands and sends back result events.

package relay

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/coder/websocket"
)

// MintResponse mirrors the relay's POST /api/sessions reply shape.
type MintResponse struct {
	WriteToken string `json:"write_token"`
	ViewToken  string `json:"view_token"`
	ViewURL    string `json:"view_url"`
	CreatedTs  int64  `json:"created_ts"`
}

// Command is what the relay sends to the bridge over the WebSocket. The
// `id` and `kind` fields are fixed; any other keys in the JSON payload
// are accessible via Extras for kind-specific arguments.
type Command struct {
	ID        string                     `json:"id"`
	Kind      string                     `json:"kind"`
	TimeoutMS int                        `json:"timeout_ms,omitempty"`
	Extras    map[string]json.RawMessage `json:"-"`
}

// UnmarshalJSON decodes a Command keeping the kind-specific fields as
// raw JSON in Extras so each capability handler can pull what it needs.
func (c *Command) UnmarshalJSON(data []byte) error {
	raw := map[string]json.RawMessage{}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	if v, ok := raw["id"]; ok {
		_ = json.Unmarshal(v, &c.ID)
		delete(raw, "id")
	}
	if v, ok := raw["kind"]; ok {
		_ = json.Unmarshal(v, &c.Kind)
		delete(raw, "kind")
	}
	if v, ok := raw["timeout_ms"]; ok {
		_ = json.Unmarshal(v, &c.TimeoutMS)
		delete(raw, "timeout_ms")
	}
	c.Extras = raw
	return nil
}

// TelemetryEvent is what the bridge sends back to the relay -- both the
// initial hello and every command_result frame.
type TelemetryEvent struct {
	Ts      int64       `json:"ts"`
	Kind    string      `json:"kind"`
	Payload interface{} `json:"payload"`
}

// Mint asks the relay to allocate a fresh session and returns the tokens.
func Mint(ctx context.Context, baseURL string) (*MintResponse, error) {
	u := strings.TrimRight(baseURL, "/") + "/api/sessions"
	req, err := http.NewRequestWithContext(ctx, "POST", u, nil)
	if err != nil {
		return nil, err
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("mint: %d %s", resp.StatusCode, strings.TrimSpace(string(body)))
	}
	var m MintResponse
	if err := json.Unmarshal(body, &m); err != nil {
		return nil, fmt.Errorf("mint: decode: %w", err)
	}
	return &m, nil
}

// Bridge is the long-lived WebSocket between this host and the relay.
type Bridge struct {
	baseURL    string
	writeToken string

	connMu sync.RWMutex
	conn   *websocket.Conn

	writeMu sync.Mutex
	helloMu sync.Mutex
	hello   *helloPayload
}

type helloPayload struct {
	hostname     string
	version      string
	capabilities []string
}

// Dial opens the WebSocket to the relay's /ws endpoint with the write
// token in the X-Write-Token header.
func Dial(ctx context.Context, baseURL, writeToken string) (*Bridge, error) {
	conn, err := dialConn(ctx, baseURL, writeToken)
	if err != nil {
		return nil, err
	}
	return &Bridge{baseURL: baseURL, writeToken: writeToken, conn: conn}, nil
}

func dialConn(ctx context.Context, baseURL, writeToken string) (*websocket.Conn, error) {
	wsURL, err := httpToWs(baseURL)
	if err != nil {
		return nil, err
	}
	wsURL = wsURL + "/ws"

	hdr := http.Header{}
	hdr.Set("X-Write-Token", writeToken)
	conn, _, err := websocket.Dial(ctx, wsURL, &websocket.DialOptions{HTTPHeader: hdr})
	if err != nil {
		return nil, fmt.Errorf("ws dial: %w", err)
	}
	conn.SetReadLimit(8 << 20) // 8 MiB
	return conn, nil
}

// SendHello announces this host's identity and command surface so the
// operator's viewer can match the command palette to this host.
func (b *Bridge) SendHello(ctx context.Context, hostname, version string, capabilities []string) error {
	caps := append([]string(nil), capabilities...)
	sort.Strings(caps)
	hello := &helloPayload{
		hostname:     hostname,
		version:      version,
		capabilities: append([]string(nil), caps...),
	}
	b.helloMu.Lock()
	b.hello = hello
	b.helloMu.Unlock()
	return b.sendHello(ctx, hello)
}

func (b *Bridge) sendHello(ctx context.Context, hello *helloPayload) error {
	if hello == nil {
		return nil
	}
	payload := map[string]interface{}{
		"hostname":         hello.hostname,
		"version":          hello.version,
		"os":               "windows",
		"protocol_version": 1,
		"capabilities":     hello.capabilities,
	}
	return b.send(ctx, "hello", payload)
}

// SendCommandResult posts the outcome of a dispatched command back to
// the relay. Payload includes the command id so the operator's viewer
// can pair it with the queued event.
func (b *Bridge) SendCommandResult(ctx context.Context, id string, ok bool, result interface{}, errMsg string, elapsedMs int64) error {
	payload := map[string]interface{}{
		"id":         id,
		"ok":         ok,
		"result":     result,
		"error":      errMsg,
		"elapsed_ms": elapsedMs,
	}
	return b.send(ctx, "command_result", payload)
}

// SendTunnelData forwards bytes that the host just read from a tunneled local
// TCP connection back to the relay, which routes them to the operator's WS.
func (b *Bridge) SendTunnelData(ctx context.Context, tunnelID, streamID string, data []byte) error {
	payload := map[string]interface{}{
		"tunnel_id":   tunnelID,
		"stream_id":   streamID,
		"data_base64": base64.StdEncoding.EncodeToString(data),
	}
	return b.send(ctx, "tunnel_data", payload)
}

// SendTunnelStreamClose tells the relay that the host-side TCP connection for
// a tunnel stream has ended (clean EOF or error).
func (b *Bridge) SendTunnelStreamClose(ctx context.Context, tunnelID, streamID, reason string) error {
	payload := map[string]interface{}{
		"tunnel_id": tunnelID,
		"stream_id": streamID,
		"reason":    reason,
	}
	return b.send(ctx, "tunnel_stream_close", payload)
}

// SendTunnelClose tells the relay the whole tunnel is finished from the host's
// perspective. Typically only used during shutdown.
func (b *Bridge) SendTunnelClose(ctx context.Context, tunnelID, reason string) error {
	payload := map[string]interface{}{
		"tunnel_id": tunnelID,
		"reason":    reason,
	}
	return b.send(ctx, "tunnel_close", payload)
}

// Recv blocks until the next Command arrives or the context cancels.
// A clean shutdown returns io.EOF.
func (b *Bridge) Recv(ctx context.Context) (*Command, error) {
	for {
		conn, err := b.currentConn()
		if err != nil {
			return nil, err
		}
		typ, data, err := conn.Read(ctx)
		if err != nil {
			if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
				return nil, err
			}
			// Treat normal closes as EOF.
			var ce websocket.CloseError
			if errors.As(err, &ce) {
				return nil, io.EOF
			}
			return nil, err
		}
		if typ != websocket.MessageText {
			continue
		}
		var c Command
		if err := json.Unmarshal(data, &c); err != nil {
			// Discard garbage frames; don't kill the loop.
			continue
		}
		return &c, nil
	}
}

// Reconnect swaps the bridge to a fresh relay WebSocket and replays the last
// hello on the same Bridge object so callers keep existing tunnel state.
func (b *Bridge) Reconnect(ctx context.Context) error {
	conn, err := dialConn(ctx, b.baseURL, b.writeToken)
	if err != nil {
		return err
	}

	b.connMu.Lock()
	old := b.conn
	b.conn = conn
	b.connMu.Unlock()
	if old != nil {
		_ = old.Close(websocket.StatusNormalClosure, "reconnect")
	}

	b.helloMu.Lock()
	hello := b.hello
	b.helloMu.Unlock()
	if err := b.sendHello(ctx, hello); err != nil {
		return fmt.Errorf("replay hello: %w", err)
	}
	return nil
}

// Close ends the WebSocket cleanly. Safe to call once.
func (b *Bridge) Close() error {
	b.connMu.Lock()
	conn := b.conn
	b.conn = nil
	b.connMu.Unlock()
	if conn == nil {
		return nil
	}
	return conn.Close(websocket.StatusNormalClosure, "bye")
}

func (b *Bridge) send(ctx context.Context, kind string, payload interface{}) error {
	ev := TelemetryEvent{
		Ts:      time.Now().UnixMilli(),
		Kind:    kind,
		Payload: payload,
	}
	var buf bytes.Buffer
	if err := json.NewEncoder(&buf).Encode(ev); err != nil {
		return err
	}
	b.writeMu.Lock()
	defer b.writeMu.Unlock()
	conn, err := b.currentConn()
	if err != nil {
		return err
	}
	return conn.Write(ctx, websocket.MessageText, bytes.TrimRight(buf.Bytes(), "\n"))
}

func (b *Bridge) currentConn() (*websocket.Conn, error) {
	b.connMu.RLock()
	conn := b.conn
	b.connMu.RUnlock()
	if conn == nil {
		return nil, errors.New("bridge is closed")
	}
	return conn, nil
}

func httpToWs(base string) (string, error) {
	u, err := url.Parse(base)
	if err != nil {
		return "", err
	}
	switch u.Scheme {
	case "http":
		u.Scheme = "ws"
	case "https":
		u.Scheme = "wss"
	default:
		return "", fmt.Errorf("unsupported scheme %q in %q", u.Scheme, base)
	}
	return u.String(), nil
}

// SPDX-License-Identifier: GPL-3.0-or-later
//
// Package relay is the client-side of the host<->relay link. The HTTP side
// mints a fresh session; the WebSocket side runs the long-lived bridge that
// receives commands and sends back result events.

package relay

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"sort"
	"strings"
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
	ID     string                     `json:"id"`
	Kind   string                     `json:"kind"`
	Extras map[string]json.RawMessage `json:"-"`
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
	conn *websocket.Conn
}

// Dial opens the WebSocket to the relay's /ws endpoint with the write
// token in the X-Write-Token header.
func Dial(ctx context.Context, baseURL, writeToken string) (*Bridge, error) {
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
	return &Bridge{conn: conn}, nil
}

// SendHello announces this host's identity and command surface so the
// operator's viewer can match the command palette to this host.
func (b *Bridge) SendHello(ctx context.Context, hostname, version string, capabilities []string) error {
	caps := append([]string(nil), capabilities...)
	sort.Strings(caps)
	payload := map[string]interface{}{
		"hostname":         hostname,
		"version":          version,
		"os":               "windows",
		"protocol_version": 1,
		"capabilities":     caps,
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

// Recv blocks until the next Command arrives or the context cancels.
// A clean shutdown returns io.EOF.
func (b *Bridge) Recv(ctx context.Context) (*Command, error) {
	for {
		typ, data, err := b.conn.Read(ctx)
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

// Close ends the WebSocket cleanly. Safe to call once.
func (b *Bridge) Close() error {
	return b.conn.Close(websocket.StatusNormalClosure, "bye")
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
	return b.conn.Write(ctx, websocket.MessageText, bytes.TrimRight(buf.Bytes(), "\n"))
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

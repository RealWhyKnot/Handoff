// SPDX-License-Identifier: GPL-3.0-or-later
package relay

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
)

func TestSendHelloIncludesProtocolAndCapabilities(t *testing.T) {
	frames := make(chan []byte, 1)
	errs := make(chan error, 1)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			errs <- err
			return
		}
		defer conn.Close(websocket.StatusNormalClosure, "")
		_, data, err := conn.Read(r.Context())
		if err != nil {
			errs <- err
			return
		}
		frames <- data
	}))
	defer server.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	conn, _, err := websocket.Dial(ctx, "ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	defer conn.Close(websocket.StatusNormalClosure, "")

	bridge := &Bridge{conn: conn}
	if err := bridge.SendHello(ctx, "host-a", "v2026.5.22.3", []string{"sys.uptime", "fs.read"}); err != nil {
		t.Fatalf("SendHello: %v", err)
	}

	var frame []byte
	select {
	case frame = <-frames:
	case err := <-errs:
		t.Fatalf("server error: %v", err)
	case <-ctx.Done():
		t.Fatal("timed out waiting for hello frame")
	}

	var event struct {
		Kind    string `json:"kind"`
		Payload struct {
			Hostname        string   `json:"hostname"`
			Version         string   `json:"version"`
			OS              string   `json:"os"`
			ProtocolVersion int      `json:"protocol_version"`
			Capabilities    []string `json:"capabilities"`
		} `json:"payload"`
	}
	if err := json.Unmarshal(frame, &event); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	if event.Kind != "hello" {
		t.Fatalf("kind = %q, want hello", event.Kind)
	}
	if event.Payload.Hostname != "host-a" || event.Payload.Version != "v2026.5.22.3" || event.Payload.OS != "windows" {
		t.Fatalf("payload identity = %#v", event.Payload)
	}
	if event.Payload.ProtocolVersion != 1 {
		t.Fatalf("protocol_version = %d, want 1", event.Payload.ProtocolVersion)
	}
	wantCaps := []string{"fs.read", "sys.uptime"}
	if !reflect.DeepEqual(event.Payload.Capabilities, wantCaps) {
		t.Fatalf("capabilities = %#v, want %#v", event.Payload.Capabilities, wantCaps)
	}
}

func TestCommandUnmarshalCapturesTimeoutOutsideExtras(t *testing.T) {
	var cmd Command
	if err := json.Unmarshal([]byte(`{"id":"cmd-1","kind":"ps.exec","timeout_ms":1500,"script":"Start-Sleep 30"}`), &cmd); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	if cmd.ID != "cmd-1" || cmd.Kind != "ps.exec" || cmd.TimeoutMS != 1500 {
		t.Fatalf("command = %#v", cmd)
	}
	if _, ok := cmd.Extras["timeout_ms"]; ok {
		t.Fatal("timeout_ms should not be forwarded to capability extras")
	}
	if _, ok := cmd.Extras["script"]; !ok {
		t.Fatal("script extra missing")
	}
}

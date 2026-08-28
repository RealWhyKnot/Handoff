// SPDX-License-Identifier: GPL-3.0-or-later
package relay

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
	"github.com/coder/websocket"
)

type capturedFrame struct {
	token string
	data  []byte
}

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
	if err := bridge.SendHello(ctx, "host-a", "v2026.5.22.3", []string{"sys.uptime", "fs.read"}, nil); err != nil {
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
	if event.Payload.ProtocolVersion != 2 {
		t.Fatalf("protocol_version = %d, want 2", event.Payload.ProtocolVersion)
	}
	wantCaps := []string{"fs.read", "sys.uptime"}
	if !reflect.DeepEqual(event.Payload.Capabilities, wantCaps) {
		t.Fatalf("capabilities = %#v, want %#v", event.Payload.Capabilities, wantCaps)
	}
}

func TestSendHelloAdvertisesCommandSpecsWithoutChangingCapabilities(t *testing.T) {
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

	specs := []dispatch.Spec{{
		Kind: "app.list",
		Params: []dispatch.Param{
			{Name: "limit", Type: dispatch.ParamInt, Default: 300, Min: dispatch.IntPtr(1), Max: dispatch.IntPtr(5000)},
		},
	}}
	if err := bridge.SendHello(ctx, "host-a", "v2026.5.22.3", []string{"app.list"}, specs); err != nil {
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
		Payload struct {
			Capabilities []string        `json:"capabilities"`
			CommandSpecs []dispatch.Spec `json:"command_specs"`
		} `json:"payload"`
	}
	if err := json.Unmarshal(frame, &event); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}

	// An older relay filters capabilities by element type, so it must stay a
	// flat string array no matter what the schema alongside it carries.
	if !reflect.DeepEqual(event.Payload.Capabilities, []string{"app.list"}) {
		t.Fatalf("capabilities = %#v", event.Payload.Capabilities)
	}
	if len(event.Payload.CommandSpecs) != 1 || event.Payload.CommandSpecs[0].Kind != "app.list" {
		t.Fatalf("command_specs = %#v", event.Payload.CommandSpecs)
	}
	p := event.Payload.CommandSpecs[0].Params
	if len(p) != 1 || p[0].Name != "limit" || p[0].Max == nil || *p[0].Max != 5000 {
		t.Fatalf("params did not round-trip: %#v", p)
	}
}

func TestHelloStaysUnderTheFrameBudget(t *testing.T) {
	specs := make([]dispatch.Spec, 0, 80)
	for i := 0; i < 80; i++ {
		specs = append(specs, dispatch.Spec{
			Kind:        "group.kind-with-a-fairly-long-name",
			Label:       "A reasonably descriptive label",
			Description: "A one-line description of what this command does on the host.",
			Params: []dispatch.Param{
				{Name: "path", Type: dispatch.ParamString, Required: true, Description: "Absolute path to read."},
				{Name: "limit", Type: dispatch.ParamInt, Default: 200, Min: dispatch.IntPtr(1), Max: dispatch.IntPtr(2000)},
				{Name: "mode", Type: dispatch.ParamEnum, Enum: []string{"contains", "prefix", "regex"}},
			},
		})
	}
	enc, err := json.Marshal(specs)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if len(enc) > 64*1024 {
		t.Fatalf("command_specs is %d bytes; keep it under 64 KiB", len(enc))
	}
}

func TestReconnectSwapsConnAndReplaysHello(t *testing.T) {
	frames := make(chan capturedFrame, 2)
	errs := make(chan error, 2)
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
		frames <- capturedFrame{token: r.Header.Get("X-Write-Token"), data: data}
	}))
	defer server.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	bridge, err := Dial(ctx, server.URL, "write-token")
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	defer bridge.Close()
	if err := bridge.SendHello(ctx, "host-a", "v2026.6.17.2", []string{"sys.info", "tunnel.open"}, nil); err != nil {
		t.Fatalf("SendHello: %v", err)
	}
	first := readCapturedFrame(t, ctx, frames, errs)

	if err := bridge.Reconnect(ctx); err != nil {
		t.Fatalf("Reconnect: %v", err)
	}
	second := readCapturedFrame(t, ctx, frames, errs)

	for i, frame := range []capturedFrame{first, second} {
		if frame.token != "write-token" {
			t.Fatalf("frame %d token = %q, want write-token", i, frame.token)
		}
		var event struct {
			Kind    string `json:"kind"`
			Payload struct {
				Hostname     string   `json:"hostname"`
				Version      string   `json:"version"`
				Capabilities []string `json:"capabilities"`
			} `json:"payload"`
		}
		if err := json.Unmarshal(frame.data, &event); err != nil {
			t.Fatalf("frame %d unmarshal: %v", i, err)
		}
		if event.Kind != "hello" || event.Payload.Hostname != "host-a" || event.Payload.Version != "v2026.6.17.2" {
			t.Fatalf("frame %d event = %#v", i, event)
		}
		wantCaps := []string{"sys.info", "tunnel.open"}
		if !reflect.DeepEqual(event.Payload.Capabilities, wantCaps) {
			t.Fatalf("frame %d capabilities = %#v, want %#v", i, event.Payload.Capabilities, wantCaps)
		}
	}
}

func TestReconnectHonorsCanceledContext(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	bridge := &Bridge{baseURL: "http://127.0.0.1:1", writeToken: "write-token"}
	if err := bridge.Reconnect(ctx); err == nil {
		t.Fatal("Reconnect succeeded with canceled context")
	}
}

func readCapturedFrame(t *testing.T, ctx context.Context, frames <-chan capturedFrame, errs <-chan error) capturedFrame {
	t.Helper()
	select {
	case frame := <-frames:
		return frame
	case err := <-errs:
		t.Fatalf("server error: %v", err)
	case <-ctx.Done():
		t.Fatal("timed out waiting for frame")
	}
	return capturedFrame{}
}

func TestSendTunnelDataEncodesBase64(t *testing.T) {
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
	if err := bridge.SendTunnelData(ctx, "tn1", "s1", []byte("hello")); err != nil {
		t.Fatalf("SendTunnelData: %v", err)
	}

	var frame []byte
	select {
	case frame = <-frames:
	case err := <-errs:
		t.Fatalf("server error: %v", err)
	case <-ctx.Done():
		t.Fatal("timed out waiting for tunnel_data frame")
	}

	var event struct {
		Kind    string `json:"kind"`
		Payload struct {
			TunnelID string `json:"tunnel_id"`
			StreamID string `json:"stream_id"`
			DataB64  string `json:"data_base64"`
		} `json:"payload"`
	}
	if err := json.Unmarshal(frame, &event); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}
	if event.Kind != "tunnel_data" {
		t.Fatalf("kind = %q, want tunnel_data", event.Kind)
	}
	if event.Payload.TunnelID != "tn1" || event.Payload.StreamID != "s1" {
		t.Fatalf("payload identity = %#v", event.Payload)
	}
	if event.Payload.DataB64 != "aGVsbG8=" {
		t.Fatalf("data_base64 = %q, want aGVsbG8=", event.Payload.DataB64)
	}
}

func TestSendTunnelStreamCloseAndCloseFrames(t *testing.T) {
	frames := make(chan []byte, 2)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer conn.Close(websocket.StatusNormalClosure, "")
		for i := 0; i < 2; i++ {
			_, data, err := conn.Read(r.Context())
			if err != nil {
				return
			}
			frames <- data
		}
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

	if err := bridge.SendTunnelStreamClose(ctx, "tn1", "s1", "eof"); err != nil {
		t.Fatalf("SendTunnelStreamClose: %v", err)
	}
	if err := bridge.SendTunnelClose(ctx, "tn1", "host shutdown"); err != nil {
		t.Fatalf("SendTunnelClose: %v", err)
	}

	for i, want := range []string{"tunnel_stream_close", "tunnel_close"} {
		select {
		case data := <-frames:
			var ev struct {
				Kind string `json:"kind"`
			}
			if err := json.Unmarshal(data, &ev); err != nil {
				t.Fatalf("frame %d unmarshal: %v", i, err)
			}
			if ev.Kind != want {
				t.Fatalf("frame %d kind = %q, want %q", i, ev.Kind, want)
			}
		case <-ctx.Done():
			t.Fatalf("frame %d timed out", i)
		}
	}
}

func setKeepaliveTiming(t *testing.T, interval, timeout time.Duration) {
	t.Helper()
	oldInterval, oldTimeout := pingInterval, pingTimeout
	pingInterval, pingTimeout = interval, timeout
	t.Cleanup(func() { pingInterval, pingTimeout = oldInterval, oldTimeout })
}

// Accepts the WS then never Reads, so pings are never answered -- the
// client-visible shape of a half-open TCP.
func newSilentServer(t *testing.T) *httptest.Server {
	t.Helper()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer conn.CloseNow()
		<-r.Context().Done()
	}))
	t.Cleanup(server.Close)
	return server
}

func TestKeepaliveDetectsHalfOpenConn(t *testing.T) {
	setKeepaliveTiming(t, 50*time.Millisecond, 200*time.Millisecond)
	server := newSilentServer(t)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	bridge, err := Dial(ctx, server.URL, "write-token")
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	defer bridge.Close()

	if _, err := bridge.Recv(ctx); !errors.Is(err, errBridgeStale) {
		t.Fatalf("Recv err = %v, want errBridgeStale", err)
	}
}

func TestReconnectStopsOldKeepalive(t *testing.T) {
	setKeepaliveTiming(t, 50*time.Millisecond, 200*time.Millisecond)
	silent := newSilentServer(t)
	// Reads (so pings are answered) and sends one command after ~8 ping
	// cycles; a conn wrongly marked stale can never deliver it.
	pongs := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer conn.CloseNow()
		go func() {
			time.Sleep(400 * time.Millisecond)
			_ = conn.Write(r.Context(), websocket.MessageText, []byte(`{"id":"c1","kind":"noop"}`))
		}()
		for {
			if _, _, err := conn.Read(r.Context()); err != nil {
				return
			}
		}
	}))
	defer pongs.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	bridge, err := Dial(ctx, silent.URL, "write-token")
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	defer bridge.Close()
	if _, err := bridge.Recv(ctx); !errors.Is(err, errBridgeStale) {
		t.Fatalf("first Recv err = %v, want errBridgeStale", err)
	}

	bridge.baseURL = pongs.URL
	if err := bridge.Reconnect(ctx); err != nil {
		t.Fatalf("Reconnect: %v", err)
	}

	cmd, err := bridge.Recv(ctx)
	if err != nil {
		t.Fatalf("second Recv err = %v, want command", err)
	}
	if cmd.ID != "c1" || cmd.Kind != "noop" {
		t.Fatalf("second Recv command = %#v", cmd)
	}
}

func TestRecvPreservesCloseCode(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		_ = conn.Close(websocket.StatusGoingAway, "cf timeout")
	}))
	defer server.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	conn, _, err := websocket.Dial(ctx, "ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatalf("Dial: %v", err)
	}
	bridge := &Bridge{conn: conn}

	_, err = bridge.Recv(ctx)
	if !errors.Is(err, io.EOF) {
		t.Fatalf("Recv err = %v, want io.EOF via errors.Is", err)
	}
	if !strings.Contains(err.Error(), "1001") || !strings.Contains(err.Error(), "cf timeout") {
		t.Fatalf("Recv err = %q, want close code 1001 and reason", err)
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

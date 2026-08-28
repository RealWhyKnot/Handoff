// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
	"github.com/RealWhyKnot/Handoff/internal/relay"
)

// Full-stack check against a running relay: this process acts as the host
// (real bridge, real tunnel engine), the relay serves its web proxy, and a
// local HTTP server stands in for a device on the LAN. Only the consent click
// is stubbed. Set HANDOFF_E2E_RELAY to the relay base URL to run it.
//
//	HANDOFF_E2E_RELAY=http://127.0.0.1:5099 go test ./internal/capabilities/ -run E2EWebProxy -v
func TestE2EWebProxyServesLocalSite(t *testing.T) {
	base := strings.TrimRight(os.Getenv("HANDOFF_E2E_RELAY"), "/")
	if base == "" {
		t.Skip("set HANDOFF_E2E_RELAY to run the live relay end-to-end check")
	}

	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return true, nil })

	const marker = "ROUTER_ADMIN_OK"
	device := startFakeDevice(t, marker)

	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	mint, err := relay.Mint(ctx, base)
	if err != nil {
		t.Fatalf("mint: %v", err)
	}
	bridge, err := relay.Dial(ctx, base, mint.WriteToken)
	if err != nil {
		t.Fatalf("dial bridge: %v", err)
	}
	defer bridge.Close()

	router := dispatch.New()
	RegisterAll(router, bridge)
	// The stub above is installed before RegisterAll resets the gate, so put it back.
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return true, nil })

	if err := bridge.SendHello(ctx, "e2e-host", "v2026.5.22.3", router.Kinds(), router.Specs()); err != nil {
		t.Fatalf("hello: %v", err)
	}

	go runHostLoop(ctx, bridge, router)
	waitForAdvertisedCapabilities(t, ctx, base, mint.ViewToken)

	proxyURL := openWebProxy(t, ctx, base, mint.ViewToken, device.port)

	body, headers := getWithRetry(t, ctx, base+proxyURL)
	if !strings.Contains(body, marker) {
		t.Fatalf("proxied page missing %q; got %q", marker, truncate(body, 400))
	}
	if !strings.Contains(body, "<base href=\""+proxyURL+"\">") {
		t.Fatalf("proxied page missing injected base href; got %q", truncate(body, 400))
	}
	if headers.Get("X-Frame-Options") != "" {
		t.Fatalf("X-Frame-Options leaked to the browser: %q", headers.Get("X-Frame-Options"))
	}
	// The device's own cookies must stay server-side. Edge infrastructure may add
	// unrelated cookies of its own (node affinity), so check for this device's.
	for _, cookie := range headers.Values("Set-Cookie") {
		if strings.Contains(cookie, deviceCookie) {
			t.Fatalf("device cookie leaked to the browser: %q", cookie)
		}
	}

	// A relative asset must resolve through the proxy too.
	css, _ := getWithRetry(t, ctx, base+proxyURL+"style.css")
	if !strings.Contains(css, "font-family") {
		t.Fatalf("proxied stylesheet wrong: %q", truncate(css, 200))
	}

	// A payload larger than one frame proves chunking and ordering hold.
	big, _ := getWithRetry(t, ctx, base+proxyURL+"big")
	if len(big) != 3*1024*1024 {
		t.Fatalf("large body = %d bytes, want %d", len(big), 3*1024*1024)
	}
	if strings.Trim(big, "x") != "" {
		t.Fatal("large body was corrupted in transit")
	}
}

// Live check of the operator CLI: this process hosts an approved tunnel, then
// the real handoff binary forwards a local port through the relay to the fake
// device. Set HANDOFF_E2E_RELAY and HANDOFF_E2E_BIN (path to handoff.exe).
func TestE2EOperatorCLIForwardsLocalSite(t *testing.T) {
	base := strings.TrimRight(os.Getenv("HANDOFF_E2E_RELAY"), "/")
	bin := os.Getenv("HANDOFF_E2E_BIN")
	if base == "" || bin == "" {
		t.Skip("set HANDOFF_E2E_RELAY and HANDOFF_E2E_BIN to run the operator CLI check")
	}

	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return true, nil })

	const marker = "ROUTER_ADMIN_OK"
	device := startFakeDevice(t, marker)

	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	mint, err := relay.Mint(ctx, base)
	if err != nil {
		t.Fatalf("mint: %v", err)
	}
	bridge, err := relay.Dial(ctx, base, mint.WriteToken)
	if err != nil {
		t.Fatalf("dial bridge: %v", err)
	}
	defer bridge.Close()

	router := dispatch.New()
	RegisterAll(router, bridge)
	withSessionRiskPrompt(t, func(context.Context, riskRequest) (bool, error) { return true, nil })
	if err := bridge.SendHello(ctx, "e2e-host", "v2026.5.22.3", router.Kinds(), router.Specs()); err != nil {
		t.Fatalf("hello: %v", err)
	}
	go runHostLoop(ctx, bridge, router)
	waitForAdvertisedCapabilities(t, ctx, base, mint.ViewToken)

	connectToken := openTunnel(t, ctx, base, mint.ViewToken, device.port)

	localPort := freePort(t)
	cmd := exec.CommandContext(ctx, bin, "tunnel", connectToken,
		"--relay", base, "--local-port", strconv.Itoa(localPort))
	var out strings.Builder
	cmd.Stdout = &out
	cmd.Stderr = &out
	if err := cmd.Start(); err != nil {
		t.Fatalf("start operator cli: %v", err)
	}
	t.Cleanup(func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
	})

	body, _ := getWithRetry(t, ctx, fmt.Sprintf("http://127.0.0.1:%d/", localPort))
	if !strings.Contains(body, marker) {
		t.Fatalf("page through operator CLI missing %q; got %q\ncli output:\n%s",
			marker, truncate(body, 300), out.String())
	}
}

func openTunnel(t *testing.T, ctx context.Context, base, viewToken string, port int) string {
	t.Helper()
	payload := fmt.Sprintf(`{"local_port":%d,"local_host":"127.0.0.1"}`, port)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		base+"/api/sessions/"+viewToken+"/tunnel", strings.NewReader(payload))
	if err != nil {
		t.Fatalf("build tunnel request: %v", err)
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatalf("tunnel request: %v", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("tunnel status %d: %s", resp.StatusCode, body)
	}
	var out struct {
		ConnectToken string `json:"connect_token"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		t.Fatalf("decode tunnel response: %v", err)
	}
	if out.ConnectToken == "" {
		t.Fatalf("no connect token in %s", body)
	}
	return out.ConnectToken
}

func freePort(t *testing.T) int {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	port := listener.Addr().(*net.TCPAddr).Port
	_ = listener.Close()
	return port
}

type fakeDevice struct{ port int }

const deviceCookie = "handoff_e2e_device_secret"

func startFakeDevice(t *testing.T, marker string) fakeDevice {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	mux := http.NewServeMux()
	mux.HandleFunc("/style.css", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/css")
		_, _ = io.WriteString(w, "body{font-family:sans-serif}")
	})
	mux.HandleFunc("/big", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/octet-stream")
		_, _ = w.Write([]byte(strings.Repeat("x", 3*1024*1024)))
	})
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/html")
		w.Header().Set("X-Frame-Options", "DENY")
		w.Header().Set("Set-Cookie", "sess="+deviceCookie+"; HttpOnly")
		_, _ = io.WriteString(w,
			`<html><head><title>Router</title><link rel="stylesheet" href="style.css"></head>`+
				`<body><h1>`+marker+`</h1></body></html>`)
	})
	server := &http.Server{Handler: mux}
	go func() { _ = server.Serve(listener) }()
	t.Cleanup(func() { _ = server.Close() })
	return fakeDevice{port: listener.Addr().(*net.TCPAddr).Port}
}

// runHostLoop mirrors cmd.New's receive loop: tunnel frames go to the ordered
// path, everything else to the dispatcher.
func runHostLoop(ctx context.Context, bridge *relay.Bridge, router *dispatch.Router) {
	for {
		cmd, err := bridge.Recv(ctx)
		if err != nil {
			return
		}
		if IsTunnelFrameKind(cmd.Kind) {
			EnqueueTunnelFrame(cmd.Kind, cmd.Extras)
			continue
		}
		go func(c *relay.Command) {
			out := router.Dispatch(ctx, c.Kind, c.Extras)
			_ = bridge.SendCommandResult(ctx, c.ID, out)
		}(cmd)
	}
}

func waitForAdvertisedCapabilities(t *testing.T, ctx context.Context, base, viewToken string) {
	t.Helper()
	deadline := time.Now().Add(15 * time.Second)
	for {
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, base+"/api/sessions/"+viewToken+"/meta", nil)
		if err != nil {
			t.Fatalf("build meta request: %v", err)
		}
		resp, err := http.DefaultClient.Do(req)
		if err == nil {
			body, _ := io.ReadAll(resp.Body)
			resp.Body.Close()
			var meta struct {
				Source string `json:"host_capabilities_source"`
			}
			_ = json.Unmarshal(body, &meta)
			if meta.Source == "advertised" {
				return
			}
		}
		if time.Now().After(deadline) {
			t.Fatal("relay never registered the host capabilities")
		}
		time.Sleep(100 * time.Millisecond)
	}
}

func openWebProxy(t *testing.T, ctx context.Context, base, viewToken string, port int) string {
	t.Helper()
	payload := fmt.Sprintf(`{"local_port":%d,"local_host":"127.0.0.1"}`, port)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		base+"/api/sessions/"+viewToken+"/webproxy", strings.NewReader(payload))
	if err != nil {
		t.Fatalf("build webproxy request: %v", err)
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatalf("webproxy request: %v", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("webproxy status %d: %s", resp.StatusCode, body)
	}
	var out struct {
		URL string `json:"url"`
	}
	if err := json.Unmarshal(body, &out); err != nil {
		t.Fatalf("decode webproxy response: %v", err)
	}
	if out.URL == "" {
		t.Fatalf("no proxy url in %s", body)
	}
	return out.URL
}

// The tunnel is Pending until the host answers tunnel.open, so 503 is expected
// briefly right after the request.
func getWithRetry(t *testing.T, ctx context.Context, url string) (string, http.Header) {
	t.Helper()
	deadline := time.Now().Add(30 * time.Second)
	for {
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
		if err != nil {
			t.Fatalf("build request: %v", err)
		}
		resp, err := http.DefaultClient.Do(req)
		if err == nil {
			body, _ := io.ReadAll(resp.Body)
			resp.Body.Close()
			if resp.StatusCode == http.StatusOK {
				return string(body), resp.Header
			}
			if time.Now().After(deadline) {
				t.Fatalf("GET %s status %d: %s", url, resp.StatusCode, truncate(string(body), 300))
			}
		} else if time.Now().After(deadline) {
			t.Fatalf("GET %s: %v", url, err)
		}
		time.Sleep(250 * time.Millisecond)
	}
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}

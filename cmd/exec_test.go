// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestBuildExecPayloadTypesValues(t *testing.T) {
	payload, err := buildExecPayload("fs.search", argList{
		"limit=50",
		"recursive=true",
		`path=C:\Temp`,
		"pattern=*.log",
	}, "")
	if err != nil {
		t.Fatal(err)
	}

	if payload["kind"] != "fs.search" {
		t.Fatalf("kind = %v", payload["kind"])
	}
	// JSON first so numbers and booleans arrive typed, string fallback so a
	// Windows path is not mangled into something unparseable.
	if n, ok := payload["limit"].(float64); !ok || n != 50 {
		t.Fatalf("limit = %#v, want the number 50", payload["limit"])
	}
	if b, ok := payload["recursive"].(bool); !ok || !b {
		t.Fatalf("recursive = %#v, want true", payload["recursive"])
	}
	if s, ok := payload["path"].(string); !ok || s != `C:\Temp` {
		t.Fatalf("path = %#v, want the literal path", payload["path"])
	}
	if s, ok := payload["pattern"].(string); !ok || s != "*.log" {
		t.Fatalf("pattern = %#v", payload["pattern"])
	}
}

func TestBuildExecPayloadMergesArgsJSON(t *testing.T) {
	payload, err := buildExecPayload("app.list", argList{"limit=5"}, `{"name_prefix":"Micro","limit":99}`)
	if err != nil {
		t.Fatal(err)
	}
	if payload["name_prefix"] != "Micro" {
		t.Fatalf("name_prefix = %v", payload["name_prefix"])
	}
	// An explicit --arg is the more specific instruction, so it wins.
	if n, ok := payload["limit"].(float64); !ok || n != 5 {
		t.Fatalf("limit = %#v, want --arg to win over --args-json", payload["limit"])
	}
}

func TestBuildExecPayloadRejectsBadInput(t *testing.T) {
	if _, err := buildExecPayload("x", argList{"noequals"}, ""); err == nil {
		t.Fatal("expected an error for an --arg without =")
	}
	if _, err := buildExecPayload("x", nil, "[1,2]"); err == nil {
		t.Fatal("expected an error for non-object --args-json")
	}
}

func TestViewTokenFromAcceptsUrlOrToken(t *testing.T) {
	cases := map[string]string{
		"n1_abc":                                "n1_abc",
		"https://handoff.whyknot.dev/v/n1_abc":  "n1_abc",
		"https://handoff.whyknot.dev/v/n1_abc/": "n1_abc",
		"  n1_abc  ":                            "n1_abc",
	}
	for in, want := range cases {
		if got := viewTokenFrom(in); got != want {
			t.Fatalf("viewTokenFrom(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestExecExitCodes(t *testing.T) {
	cases := []struct {
		name     string
		status   int
		body     string
		wantExit int
	}{
		{"success", http.StatusOK, `{"ok":true,"result":{"a":1}}`, 0},
		{"host reported failure", http.StatusOK, `{"ok":false,"error":"boom"}`, execExitFailed},
		{"still running", http.StatusAccepted, `{"status":"pending","result_url":"/x"}`, execExitTimeout},
		{"cancelled", http.StatusGone, `{"error":"command cancelled"}`, 1},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				w.Header().Set("Content-Type", "application/json")
				w.WriteHeader(c.status)
				_, _ = w.Write([]byte(c.body))
			}))
			defer srv.Close()

			t.Setenv("HANDOFF_CONFIG", "")
			code := Exec([]string{"--relay", srv.URL, "--json", "n1_token", "sys.info"})
			if code != c.wantExit {
				t.Fatalf("Exec exit = %d, want %d", code, c.wantExit)
			}
		})
	}
}

func TestExecListPrintsAllowedKinds(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]interface{}{
			"ready":         true,
			"allowed_kinds": []string{"sys.info", "fs.read"},
		})
	}))
	defer srv.Close()

	t.Setenv("HANDOFF_CONFIG", "")
	if code := Exec([]string{"--relay", srv.URL, "--list", "n1_token"}); code != 0 {
		t.Fatalf("Exec --list exit = %d, want 0", code)
	}
}

func TestExecListFailsWhenNoHostIsConnected(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]interface{}{"ready": false})
	}))
	defer srv.Close()

	t.Setenv("HANDOFF_CONFIG", "")
	if code := Exec([]string{"--relay", srv.URL, "--list", "n1_token"}); code == 0 {
		t.Fatal("Exec --list should fail while no host is connected")
	}
}

func TestExecRequiresAKind(t *testing.T) {
	t.Setenv("HANDOFF_CONFIG", "")
	if code := Exec([]string{"--relay", "https://example.test", "n1_token"}); code != execExitUsage {
		t.Fatalf("exit = %d, want a usage error", code)
	}
}

func TestExecFallsBackWhenRelayHasNoRunEndpoint(t *testing.T) {
	// A relay that predates /run answers 404. Giving up there would make a new
	// client useless against a relay that has not been deployed yet.
	var sawCmd, sawResult bool
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch {
		case strings.HasSuffix(r.URL.Path, "/run"):
			w.WriteHeader(http.StatusNotFound)
			_, _ = w.Write([]byte(`{"error":"Not Found"}`))
		case strings.HasSuffix(r.URL.Path, "/cmd"):
			sawCmd = true
			_, _ = w.Write([]byte(`{"command_id":"c_fallback"}`))
		case strings.Contains(r.URL.Path, "/cmd/c_fallback"):
			sawResult = true
			_, _ = w.Write([]byte(`{"id":"c_fallback","payload":{"id":"c_fallback","ok":true,"result":{"up":1},"elapsed_ms":7}}`))
		default:
			w.WriteHeader(http.StatusNotFound)
		}
	}))
	defer srv.Close()

	t.Setenv("HANDOFF_CONFIG", "")
	if code := Exec([]string{"--relay", srv.URL, "--json", "n1_token", "sys.uptime"}); code != 0 {
		t.Fatalf("Exec exit = %d, want 0 via the fallback", code)
	}
	if !sawCmd || !sawResult {
		t.Fatalf("fallback path not used: cmd=%v result=%v", sawCmd, sawResult)
	}
}

func TestExecDoesNotFallBackForAnUnknownSession(t *testing.T) {
	// An unknown token also 404s. Retrying that against /cmd would turn a clear
	// error into a second confusing one.
	var cmdCalls int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if strings.HasSuffix(r.URL.Path, "/cmd") {
			cmdCalls++
		}
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusNotFound)
		_, _ = w.Write([]byte(`{"error":"unknown view token"}`))
	}))
	defer srv.Close()

	t.Setenv("HANDOFF_CONFIG", "")
	if code := Exec([]string{"--relay", srv.URL, "--json", "n1_token", "sys.uptime"}); code != 1 {
		t.Fatalf("Exec exit = %d, want 1", code)
	}
	if cmdCalls != 0 {
		t.Fatalf("fell back %d times for an unknown session", cmdCalls)
	}
}

func TestExecFallbackReportsHostFailure(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch {
		case strings.HasSuffix(r.URL.Path, "/run"):
			w.WriteHeader(http.StatusNotFound)
			_, _ = w.Write([]byte(`{"error":"Not Found"}`))
		case strings.HasSuffix(r.URL.Path, "/cmd"):
			_, _ = w.Write([]byte(`{"command_id":"c_bad"}`))
		default:
			_, _ = w.Write([]byte(`{"id":"c_bad","payload":{"id":"c_bad","ok":false,"error":"boom"}}`))
		}
	}))
	defer srv.Close()

	t.Setenv("HANDOFF_CONFIG", "")
	if code := Exec([]string{"--relay", srv.URL, "--json", "n1_token", "ps.exec"}); code != execExitFailed {
		t.Fatalf("Exec exit = %d, want %d", code, execExitFailed)
	}
}

// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
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

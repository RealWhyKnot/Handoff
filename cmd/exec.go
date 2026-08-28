// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"strings"
	"time"
)

// Exit codes an operator or script can branch on.
const (
	execExitUsage   = 2
	execExitTimeout = 3
	execExitFailed  = 4
)

type argList []string

func (a *argList) String() string { return strings.Join(*a, ",") }

func (a *argList) Set(v string) error {
	*a = append(*a, v)
	return nil
}

// Exec runs one command against a session and prints the result. It is the
// terminal equivalent of the viewer, for operators and scripts that would
// otherwise hand-roll HTTP calls against relay URL shapes.
func Exec(args []string) int {
	var (
		argPairs argList
		argsJSON string
		timeout  time.Duration
		list     bool
	)
	opts, rest, err := parseOptions("exec", args, func(fs *flag.FlagSet) {
		fs.Var(&argPairs, "arg", "command argument as key=value (repeatable)")
		fs.StringVar(&argsJSON, "args-json", "", "command arguments as a JSON object")
		fs.DurationVar(&timeout, "timeout", 60*time.Second, "how long to wait for the result")
		fs.BoolVar(&list, "list", false, "list the commands this session accepts")
	})
	if err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return 0
		}
		fmt.Fprintln(os.Stderr, err)
		return execExitUsage
	}

	if len(rest) < 1 {
		fmt.Fprintln(os.Stderr, "usage: handoff exec <view-url-or-token> <kind> [--arg k=v]...")
		return execExitUsage
	}
	token := viewTokenFrom(rest[0])

	ctx, cancel := context.WithTimeout(context.Background(), timeout+15*time.Second)
	defer cancel()

	if list {
		return execList(ctx, opts.Relay, token)
	}
	if len(rest) < 2 {
		fmt.Fprintln(os.Stderr, "usage: handoff exec <view-url-or-token> <kind> [--arg k=v]...")
		return execExitUsage
	}

	payload, err := buildExecPayload(rest[1], argPairs, argsJSON)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return execExitUsage
	}

	url := fmt.Sprintf("%s/api/sessions/%s/run?wait_ms=%d", opts.Relay, token, timeout.Milliseconds())
	body, status, err := postJSON(ctx, url, payload)
	if err != nil {
		fmt.Fprintln(os.Stderr, "request failed:", err)
		return 1
	}

	var reply map[string]interface{}
	if err := json.Unmarshal(body, &reply); err != nil {
		fmt.Fprintf(os.Stderr, "relay returned %d: %s\n", status, strings.TrimSpace(string(body)))
		return 1
	}

	if opts.JSON {
		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "  ")
		_ = enc.Encode(reply)
	}

	switch status {
	case http.StatusOK:
		ok, _ := reply["ok"].(bool)
		if !opts.JSON {
			printExecResult(reply, ok)
		}
		if !ok {
			return execExitFailed
		}
		return 0
	case http.StatusAccepted:
		if !opts.JSON {
			fmt.Fprintf(os.Stderr, "still running after %s; fetch it later at %v\n", timeout, reply["result_url"])
		}
		return execExitTimeout
	default:
		if !opts.JSON {
			fmt.Fprintf(os.Stderr, "relay returned %d: %v\n", status, reply["error"])
		}
		return 1
	}
}

func printExecResult(reply map[string]interface{}, ok bool) {
	if !ok {
		fmt.Fprintln(os.Stderr, "command failed:", reply["error"])
		if detail, present := reply["detail"]; present && detail != nil {
			enc, _ := json.MarshalIndent(detail, "", "  ")
			fmt.Fprintln(os.Stderr, string(enc))
		}
		return
	}
	result, present := reply["result"]
	if !present || result == nil {
		fmt.Println("ok")
		return
	}
	if s, isString := result.(string); isString {
		fmt.Println(s)
		return
	}
	enc, err := json.MarshalIndent(result, "", "  ")
	if err != nil {
		fmt.Println(result)
		return
	}
	fmt.Println(string(enc))
}

func execList(ctx context.Context, relayBase, token string) int {
	url := fmt.Sprintf("%s/api/sessions/%s/meta", relayBase, token)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		fmt.Fprintln(os.Stderr, "request failed:", err)
		return 1
	}
	defer resp.Body.Close()

	var meta struct {
		Ready        bool     `json:"ready"`
		AllowedKinds []string `json:"allowed_kinds"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&meta); err != nil {
		fmt.Fprintln(os.Stderr, "could not read session metadata:", err)
		return 1
	}
	if !meta.Ready {
		fmt.Fprintln(os.Stderr, "no host is connected to this session yet")
		return 1
	}
	for _, k := range meta.AllowedKinds {
		fmt.Println(k)
	}
	return 0
}

// buildExecPayload turns --arg pairs into a command body. Values parse as JSON
// first so numbers and booleans arrive typed, falling back to a string, which
// is what a Windows path needs.
func buildExecPayload(kind string, pairs argList, argsJSON string) (map[string]interface{}, error) {
	payload := map[string]interface{}{"kind": kind}

	if strings.TrimSpace(argsJSON) != "" {
		var parsed map[string]interface{}
		if err := json.Unmarshal([]byte(argsJSON), &parsed); err != nil {
			return nil, fmt.Errorf("--args-json is not a JSON object: %w", err)
		}
		for k, v := range parsed {
			payload[k] = v
		}
	}

	for _, pair := range pairs {
		key, value, found := strings.Cut(pair, "=")
		if !found || key == "" {
			return nil, fmt.Errorf("--arg must be key=value, got %q", pair)
		}
		var typed interface{}
		if err := json.Unmarshal([]byte(value), &typed); err == nil {
			payload[key] = typed
			continue
		}
		payload[key] = value
	}
	return payload, nil
}

func postJSON(ctx context.Context, url string, payload interface{}) ([]byte, int, error) {
	enc, err := json.Marshal(payload)
	if err != nil {
		return nil, 0, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(enc))
	if err != nil {
		return nil, 0, err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, 0, err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(resp.Body)
	return body, resp.StatusCode, err
}

// viewTokenFrom accepts either a bare token or a full view URL.
func viewTokenFrom(s string) string {
	s = strings.TrimSpace(s)
	if i := strings.LastIndex(s, "/v/"); i >= 0 {
		return strings.Trim(s[i+3:], "/")
	}
	return strings.Trim(s, "/")
}

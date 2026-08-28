// SPDX-License-Identifier: GPL-3.0-or-later
//
// Package audit writes a JSONL log of every command the host runs as
// part of a session. The host can review or share the log to verify
// what the operator did. One file per day; append-only.

package audit

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"
)

// Entry is one row in the audit log.
type Entry struct {
	Ts           string      `json:"ts"`
	SessionID    string      `json:"sid"`
	Operator     string      `json:"op"`
	Capability   string      `json:"cap"`
	Args         interface{} `json:"args"`
	Consent      string      `json:"consent"`
	ConsentScope string      `json:"consent_scope,omitempty"`
	Result       string      `json:"result"`
	ElapsedMs    int64       `json:"elapsed_ms"`
	Detail       string      `json:"detail,omitempty"`
}

// maxArgValueBytes keeps one oversized value from swamping the log. An
// fs.upload used to write its entire base64 payload into every line.
const maxArgValueBytes = 512

// TrimArgs replaces bulky values with a size note so the log stays readable
// and stays a record of what was asked for rather than a copy of the payload.
func TrimArgs(args map[string]json.RawMessage) map[string]interface{} {
	if len(args) == 0 {
		return nil
	}
	out := make(map[string]interface{}, len(args))
	for k, raw := range args {
		if len(raw) > maxArgValueBytes {
			out[k] = fmt.Sprintf("<elided %d bytes>", len(raw))
			continue
		}
		var v interface{}
		if err := json.Unmarshal(raw, &v); err != nil {
			out[k] = "<unreadable>"
			continue
		}
		out[k] = v
	}
	return out
}

// Logger is goroutine-safe; multiple capability handlers writing
// concurrently won't tear lines.
type Logger struct {
	mu  sync.Mutex
	dir string
	f   *os.File
	day string
}

// New returns a Logger backed by the standard %PROGRAMDATA% audit dir.
// If PROGRAMDATA is unset (rare), falls back to %TEMP%.
func New() (*Logger, error) { return NewInDir("") }

// NewInDir backs the logger with an explicit directory, falling back to the
// standard %PROGRAMDATA% location when empty.
func NewInDir(dir string) (*Logger, error) {
	if dir == "" {
		root := os.Getenv("PROGRAMDATA")
		if root == "" {
			root = os.TempDir()
		}
		dir = filepath.Join(root, "whyknot", "handoff", "audit")
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, fmt.Errorf("mkdir audit dir: %w", err)
	}
	return &Logger{dir: dir}, nil
}

// Dir is where entries are being written, for the session banner.
func (l *Logger) Dir() string { return l.dir }

// Write appends one entry. Ts is filled in if empty.
func (l *Logger) Write(e Entry) error {
	if e.Ts == "" {
		e.Ts = time.Now().UTC().Format("2006-01-02T15:04:05.000Z")
	}
	l.mu.Lock()
	defer l.mu.Unlock()

	day := time.Now().UTC().Format("2006-01-02")
	if l.f == nil || l.day != day {
		if l.f != nil {
			_ = l.f.Close()
		}
		path := filepath.Join(l.dir, "handoff-"+day+".jsonl")
		f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
		if err != nil {
			return fmt.Errorf("open audit file: %w", err)
		}
		l.f = f
		l.day = day
	}
	b, err := json.Marshal(e)
	if err != nil {
		return err
	}
	b = append(b, '\n')
	_, err = l.f.Write(b)
	return err
}

// Close flushes and closes the underlying file.
func (l *Logger) Close() error {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.f != nil {
		err := l.f.Close()
		l.f = nil
		return err
	}
	return nil
}

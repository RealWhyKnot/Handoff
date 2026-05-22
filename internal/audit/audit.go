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
	Ts        string      `json:"ts"`         // RFC3339 with millis, UTC
	SessionID string      `json:"sid"`        // first 8 chars of view token
	Operator  string      `json:"op"`         // remote ip seen at session start, may be empty
	Capability string     `json:"cap"`        // command kind
	Args      interface{} `json:"args"`       // command extras (may be trimmed for very large payloads)
	Consent   string      `json:"consent"`    // session | prompt_allow | prompt_deny | auto
	Result    string      `json:"result"`     // ok | err | denied
	ElapsedMs int64       `json:"elapsed_ms"`
	Detail    string      `json:"detail,omitempty"`
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
func New() (*Logger, error) {
	root := os.Getenv("PROGRAMDATA")
	if root == "" {
		root = os.TempDir()
	}
	dir := filepath.Join(root, "whyknot", "handoff", "audit")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, fmt.Errorf("mkdir audit dir: %w", err)
	}
	return &Logger{dir: dir}, nil
}

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

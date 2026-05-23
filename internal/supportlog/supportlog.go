// SPDX-License-Identifier: GPL-3.0-or-later
package supportlog

import (
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"
)

type Logger struct {
	mu sync.Mutex
	f  *os.File
}

var current *Logger

func Init() (string, error) {
	path, err := Path()
	if err != nil {
		return "", err
	}
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return path, err
	}
	current = &Logger{f: f}
	Printf("handoff start pid=%d exe=%q", os.Getpid(), mustExecutable())
	return path, nil
}

func Path() (string, error) {
	exe, err := os.Executable()
	if err != nil {
		return "", fmt.Errorf("resolve executable: %w", err)
	}
	return PathForExecutable(exe), nil
}

func PathForExecutable(exe string) string {
	dir := filepath.Dir(exe)
	base := filepath.Base(exe)
	ext := filepath.Ext(base)
	name := base[:len(base)-len(ext)]
	if name == "" || name == "." {
		name = "handoff"
	}
	return filepath.Join(dir, name+".log")
}

func Printf(format string, args ...interface{}) {
	if current == nil {
		return
	}
	current.Printf(format, args...)
}

func Close() error {
	if current == nil {
		return nil
	}
	err := current.Close()
	current = nil
	return err
}

func (l *Logger) Printf(format string, args ...interface{}) {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.f == nil {
		return
	}
	ts := time.Now().UTC().Format("2006-01-02T15:04:05.000Z")
	_, _ = fmt.Fprintf(l.f, "%s %s\n", ts, fmt.Sprintf(format, args...))
}

func (l *Logger) Close() error {
	l.mu.Lock()
	defer l.mu.Unlock()
	if l.f == nil {
		return nil
	}
	err := l.f.Close()
	l.f = nil
	return err
}

func mustExecutable() string {
	exe, err := os.Executable()
	if err != nil {
		return ""
	}
	return exe
}

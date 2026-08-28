// SPDX-License-Identifier: GPL-3.0-or-later
package supportlog

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

type Logger struct {
	mu    sync.Mutex
	f     *os.File
	path  string
	lines int
}

var current *Logger

// MaxBytes bounds the log so an unattended machine cannot fill a disk with it.
// One previous file is kept.
const MaxBytes = 2 << 20

// dirOverride lets the CLI place the log somewhere other than the default.
var dirOverride string

func SetDir(dir string) { dirOverride = strings.TrimSpace(dir) }

func Init() (string, error) {
	path, err := Path()
	if err != nil {
		return "", err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return path, err
	}
	rotateIfLarge(path)
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return path, err
	}
	current = &Logger{f: f, path: path}
	Printf("handoff start pid=%d exe=%q", os.Getpid(), mustExecutable())
	return path, nil
}

// Path is the support log location. It deliberately does NOT sit beside the
// executable: that is usually the Downloads folder, where an unrotated log
// full of session detail is the last thing anyone wants.
func Path() (string, error) {
	if dirOverride != "" {
		return filepath.Join(dirOverride, "handoff.log"), nil
	}
	base, err := os.UserCacheDir()
	if err != nil {
		exe, exeErr := os.Executable()
		if exeErr != nil {
			return "", fmt.Errorf("resolve log directory: %w", err)
		}
		return PathForExecutable(exe), nil
	}
	return filepath.Join(base, "whyknot", "handoff", "logs", "handoff.log"), nil
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

func rotateIfLarge(path string) {
	info, err := os.Stat(path)
	if err != nil || info.Size() < MaxBytes {
		return
	}
	_ = os.Remove(path + ".1")
	_ = os.Rename(path, path+".1")
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

	l.lines++
	if l.lines%256 != 0 || l.path == "" {
		return
	}
	if info, err := l.f.Stat(); err == nil && info.Size() >= MaxBytes {
		_ = l.f.Close()
		_ = os.Remove(l.path + ".1")
		_ = os.Rename(l.path, l.path+".1")
		if f, err := os.OpenFile(l.path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644); err == nil {
			l.f = f
		}
	}
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

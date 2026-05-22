// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterFs wires fs.ls and fs.read. Write-side fs.upload / fs.download
// are gated and live in fs_gated.go (deferred until v0.2).
func RegisterFs(r *dispatch.Router) {
	r.Register("fs.ls", fsLs)
	r.Register("fs.read", fsRead)
}

func fsLs(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var path string
	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &path)
	}
	if path == "" {
		return nil, fmt.Errorf("fs.ls: 'path' is required")
	}
	path = filepath.Clean(path)

	entries, err := os.ReadDir(path)
	if err != nil {
		return nil, err
	}
	out := make([]map[string]interface{}, 0, len(entries))
	for _, e := range entries {
		info, err := e.Info()
		if err != nil {
			continue
		}
		row := map[string]interface{}{
			"name": e.Name(),
			"dir":  e.IsDir(),
			"size": info.Size(),
			"mode": info.Mode().String(),
			"mtime": info.ModTime().UTC().Format("2006-01-02T15:04:05Z"),
		}
		out = append(out, row)
	}
	return map[string]interface{}{
		"path":    path,
		"entries": out,
	}, nil
}

// fsRead returns a file's bytes (base64-encoded) plus its sha256.
// Reads above 8 MiB are refused outright; reads above 1 MiB will
// require an explicit consent flag in a future revision.
func fsRead(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var path string
	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &path)
	}
	if path == "" {
		return nil, fmt.Errorf("fs.read: 'path' is required")
	}
	path = filepath.Clean(path)

	// Refuse obviously sensitive system locations regardless of any
	// future consent flag -- these paths leak credentials.
	lp := strings.ToLower(filepath.ToSlash(path))
	for _, bad := range []string{
		"/windows/system32/config/",
		"/windows/system32/configstore/",
		"/users/all users/microsoft/crypto/",
	} {
		if strings.Contains(lp, bad) {
			return nil, fmt.Errorf("fs.read: refusing to read %s (sensitive system path)", bad)
		}
	}

	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	if info.IsDir() {
		return nil, fmt.Errorf("fs.read: %s is a directory; use fs.ls", path)
	}
	const cap8MiB = 8 * 1024 * 1024
	if info.Size() > cap8MiB {
		return nil, fmt.Errorf("fs.read: file is %d bytes; cap is %d", info.Size(), cap8MiB)
	}

	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	h := sha256.New()
	buf := make([]byte, info.Size())
	if _, err := io.ReadFull(f, buf); err != nil {
		return nil, err
	}
	h.Write(buf)

	return map[string]interface{}{
		"path":     path,
		"size":     info.Size(),
		"sha256":   hex.EncodeToString(h.Sum(nil)),
		"base64":   base64.StdEncoding.EncodeToString(buf),
		"mtime":    info.ModTime().UTC().Format("2006-01-02T15:04:05Z"),
	}, nil
}

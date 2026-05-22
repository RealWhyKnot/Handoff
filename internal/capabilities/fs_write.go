// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterFsWrite wires fs.upload (operator -> host) and fs.download
// (host -> operator). v0.1.5 is single-shot, sized to fit inside the
// relay's per-command body cap (currently 2 MiB). Larger transfers
// will need chunked transport; tracked for v0.2.
func RegisterFsWrite(r *dispatch.Router) {
	r.Register("fs.upload", fsUpload)
	r.Register("fs.download", fsDownload)
	r.Register("fs.mkdir", fsMkdir)
	r.Register("fs.delete", fsDelete)
}

func fsUpload(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var (
		path      string
		dataB64   string
		expectSha string
		overwrite bool
	)
	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &path)
	}
	if v, ok := args["data_base64"]; ok {
		_ = json.Unmarshal(v, &dataB64)
	}
	if v, ok := args["sha256"]; ok {
		_ = json.Unmarshal(v, &expectSha)
	}
	if v, ok := args["overwrite"]; ok {
		_ = json.Unmarshal(v, &overwrite)
	}
	if path == "" {
		return nil, fmt.Errorf("fs.upload: 'path' is required")
	}
	if dataB64 == "" {
		return nil, fmt.Errorf("fs.upload: 'data_base64' is required")
	}
	path = filepath.Clean(path)

	if err := guardWritePath(path); err != nil {
		return nil, err
	}

	data, err := base64.StdEncoding.DecodeString(dataB64)
	if err != nil {
		return nil, fmt.Errorf("fs.upload: base64 decode: %w", err)
	}

	gotSha := sha256.Sum256(data)
	gotShaHex := hex.EncodeToString(gotSha[:])
	if expectSha != "" && !strings.EqualFold(expectSha, gotShaHex) {
		return nil, fmt.Errorf("fs.upload: sha256 mismatch (got %s, expected %s)", gotShaHex, expectSha)
	}

	if !overwrite {
		if _, err := os.Stat(path); err == nil {
			return nil, fmt.Errorf("fs.upload: %s already exists (pass overwrite=true to replace)", path)
		}
	}
	if err := requireRiskConsent(ctx, "fs.upload", "Writes operator-supplied file content to this computer. Uploaded content can replace files when overwrite=true."); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, err
	}
	if err := os.WriteFile(path, data, 0o644); err != nil {
		return nil, err
	}

	return map[string]interface{}{
		"path":   path,
		"size":   len(data),
		"sha256": gotShaHex,
	}, nil
}

// fsDownload is the operator-side companion of fs.read. Same shape;
// kept separate so future revisions can grow chunked transport on the
// download path without touching the local-debug fs.read contract.
func fsDownload(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	return fsRead(ctx, args)
}

func fsMkdir(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var path string
	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &path)
	}
	if path == "" {
		return nil, fmt.Errorf("fs.mkdir: 'path' is required")
	}
	path = filepath.Clean(path)
	if err := guardWritePath(path); err != nil {
		return nil, err
	}
	if err := requireRiskConsent(ctx, "fs.mkdir", "Creates a directory on this computer. This changes the host filesystem."); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(path, 0o755); err != nil {
		return nil, err
	}
	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	return map[string]interface{}{
		"path":  path,
		"dir":   info.IsDir(),
		"mtime": info.ModTime().UTC().Format("2006-01-02T15:04:05Z"),
	}, nil
}

func fsDelete(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var (
		path      string
		recursive bool
	)
	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &path)
	}
	if v, ok := args["recursive"]; ok {
		_ = json.Unmarshal(v, &recursive)
	}
	if path == "" {
		return nil, fmt.Errorf("fs.delete: 'path' is required")
	}
	path = filepath.Clean(path)
	if err := guardDeletePath(path); err != nil {
		return nil, err
	}
	info, err := os.Lstat(path)
	if err != nil {
		return nil, err
	}
	if info.IsDir() && !recursive {
		return nil, fmt.Errorf("fs.delete: %s is a directory (pass recursive=true to remove it)", path)
	}
	if err := requireRiskConsent(ctx, "fs.delete", "Deletes a file or directory from this computer. Directory deletion requires recursive=true and cannot be undone by Handoff."); err != nil {
		return nil, err
	}
	if info.IsDir() {
		err = os.RemoveAll(path)
	} else {
		err = os.Remove(path)
	}
	if err != nil {
		return nil, err
	}
	return map[string]interface{}{
		"path":      path,
		"dir":       info.IsDir(),
		"size":      info.Size(),
		"recursive": recursive,
		"deleted":   true,
	}, nil
}

// guardWritePath refuses paths that would obviously break the host or
// leak secrets if overwritten. The list is intentionally short -- the
// host is opting in by accepting the session at all.
func guardWritePath(p string) error {
	return guardMutablePath("fs.upload", p)
}

func guardDeletePath(p string) error {
	if !filepath.IsAbs(p) {
		return fmt.Errorf("fs.delete: path must be absolute")
	}
	if isProtectedRoot(p) {
		return fmt.Errorf("fs.delete: refusing to delete protected root %s", p)
	}
	return guardMutablePath("fs.delete", p)
}

func guardMutablePath(kind, p string) error {
	lp := strings.ToLower(filepath.ToSlash(filepath.Clean(p)))
	if strings.HasPrefix(lp, "c:") {
		lp = lp[2:]
	}
	lp = strings.TrimRight(lp, "/")
	for _, bad := range []string{
		"/windows/system32",
		"/windows/syswow64",
		"/program files",
		"/program files (x86)",
	} {
		if lp == bad || strings.HasPrefix(lp, bad+"/") {
			return fmt.Errorf("%s: refusing to modify under %s", kind, bad)
		}
	}
	return nil
}

func isProtectedRoot(p string) bool {
	clean := filepath.Clean(p)
	vol := filepath.VolumeName(clean)
	if vol != "" {
		rest := strings.TrimPrefix(clean, vol)
		if rest == "" || rest == string(filepath.Separator) {
			return true
		}
	}

	protected := []string{
		os.Getenv("SystemRoot"),
		os.Getenv("ProgramFiles"),
		os.Getenv("ProgramFiles(x86)"),
	}
	if home, err := os.UserHomeDir(); err == nil {
		protected = append(protected, home)
	}
	for _, root := range protected {
		if root == "" {
			continue
		}
		if strings.EqualFold(filepath.Clean(root), clean) {
			return true
		}
	}
	return false
}

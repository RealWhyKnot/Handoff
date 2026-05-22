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

// guardWritePath refuses paths that would obviously break the host or
// leak secrets if overwritten. The list is intentionally short -- the
// host is opting in by accepting the session at all.
func guardWritePath(p string) error {
	lp := strings.ToLower(filepath.ToSlash(p))
	for _, bad := range []string{
		"/windows/system32/",
		"/windows/syswow64/",
		"/program files/",
		"/program files (x86)/",
	} {
		if strings.HasPrefix(lp, "c:") {
			lp = lp[2:]
		}
		if strings.HasPrefix(lp, bad) {
			return fmt.Errorf("fs.upload: refusing to write under %s", bad)
		}
	}
	return nil
}

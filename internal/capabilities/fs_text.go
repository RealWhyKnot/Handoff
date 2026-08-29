// SPDX-License-Identifier: GPL-3.0-or-later
//
// fs.write writes text directly. The only way to put a file on the host used
// to be fs.upload, which takes base64: a human operator cannot type that by
// hand and an agent gets the encoding wrong, for what is usually a three-line
// script or one changed config value.

package capabilities

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

const fsWriteCap = 1 << 20

func RegisterFsText(r *dispatch.Router) {
	r.RegisterSpec(dispatch.Spec{
		Kind:        "fs.write",
		Label:       "Write text file",
		Description: "Write or append text to a file on the host.",
		Risky:       true,
		Params: []dispatch.Param{
			{Name: "path", Type: dispatch.ParamString, Required: true, Description: "Absolute path to write."},
			{Name: "content", Type: dispatch.ParamString, Required: true, MaxBytes: fsWriteCap},
			{Name: "encoding", Type: dispatch.ParamEnum, Enum: []string{"utf8", "utf8bom", "ascii"}, Default: "utf8"},
			{Name: "newline", Type: dispatch.ParamEnum, Enum: []string{"lf", "crlf"}, Default: "crlf"},
			{Name: "overwrite", Type: dispatch.ParamBool, Default: false},
			{Name: "append", Type: dispatch.ParamBool, Default: false},
		},
	}, fsWrite)
}

func fsWrite(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var (
		path      string
		content   string
		encoding  = "utf8"
		newline   = "crlf"
		overwrite bool
		appendTo  bool
	)
	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &path)
	}
	if v, ok := args["content"]; ok {
		_ = json.Unmarshal(v, &content)
	}
	if v, ok := args["encoding"]; ok {
		_ = json.Unmarshal(v, &encoding)
	}
	if v, ok := args["newline"]; ok {
		_ = json.Unmarshal(v, &newline)
	}
	if v, ok := args["overwrite"]; ok {
		_ = json.Unmarshal(v, &overwrite)
	}
	if v, ok := args["append"]; ok {
		_ = json.Unmarshal(v, &appendTo)
	}

	path = strings.TrimSpace(path)
	if path == "" {
		return nil, fmt.Errorf("fs.write: 'path' is required")
	}
	if !filepath.IsAbs(path) {
		return nil, fmt.Errorf("fs.write: 'path' must be absolute")
	}
	path = filepath.Clean(path)
	if len(content) > fsWriteCap {
		return nil, fmt.Errorf("fs.write: content is %d bytes; cap is %d", len(content), fsWriteCap)
	}
	if err := guardWritePath(path); err != nil {
		return nil, err
	}

	// Ask before probing, so a refusal cannot be used to discover which paths
	// exist on the host.
	if err := requireRiskConsent(ctx, "fs.write", fmt.Sprintf("Writes %d bytes of text to %s on this computer.", len(content), path)); err != nil {
		return nil, err
	}

	body := content
	if newline == "crlf" {
		body = strings.ReplaceAll(strings.ReplaceAll(body, "\r\n", "\n"), "\n", "\r\n")
	} else {
		body = strings.ReplaceAll(body, "\r\n", "\n")
	}

	data := []byte(body)
	if encoding == "ascii" {
		for i := 0; i < len(data); i++ {
			if data[i] > 0x7f {
				return nil, fmt.Errorf("fs.write: content is not ascii at byte %d", i)
			}
		}
	}

	_, statErr := os.Stat(path)
	existed := statErr == nil
	if existed && !overwrite && !appendTo {
		return nil, fmt.Errorf("fs.write: %s already exists (pass overwrite=true or append=true)", path)
	}

	if encoding == "utf8bom" && !(appendTo && existed) {
		data = append([]byte{0xEF, 0xBB, 0xBF}, data...)
	}

	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, err
	}

	if appendTo {
		f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
		if err != nil {
			return nil, err
		}
		_, err = f.Write(data)
		if cerr := f.Close(); err == nil {
			err = cerr
		}
		if err != nil {
			return nil, err
		}
	} else if err := os.WriteFile(path, data, 0o644); err != nil {
		return nil, err
	}

	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	sum := sha256.Sum256(data)
	return map[string]interface{}{
		"path":     path,
		"size":     info.Size(),
		"written":  len(data),
		"sha256":   hex.EncodeToString(sum[:]),
		"created":  !existed,
		"appended": appendTo,
		"encoding": encoding,
		"newline":  newline,
	}, nil
}

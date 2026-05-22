// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterFs wires fs.ls and fs.read. Mutating filesystem commands live in
// fs_write.go and require session consent before changing the host.
func RegisterFs(r *dispatch.Router) {
	r.Register("fs.ls", fsLs)
	r.Register("fs.read", fsRead)
	r.Register("fs.search", fsSearch)
	r.Register("fs.head", fsHead)
	r.Register("fs.tail", fsTail)
	r.Register("fs.stat", fsStat)
	r.Register("fs.tree", fsTree)
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
			"name":  e.Name(),
			"dir":   e.IsDir(),
			"size":  info.Size(),
			"mode":  info.Mode().String(),
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
		"path":   path,
		"size":   info.Size(),
		"sha256": hex.EncodeToString(h.Sum(nil)),
		"base64": base64.StdEncoding.EncodeToString(buf),
		"mtime":  info.ModTime().UTC().Format("2006-01-02T15:04:05Z"),
	}, nil
}

func fsSearch(_ context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var (
		searchPath    string
		pattern       string = "*"
		maxResults    int    = 200
		maxDepth      int    = 4
		includeDirs   bool
		includeHidden bool
	)

	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &searchPath)
	}
	if searchPath == "" {
		return nil, fmt.Errorf("fs.search: 'path' is required")
	}
	searchPath = filepath.Clean(searchPath)
	if !filepath.IsAbs(searchPath) {
		return nil, fmt.Errorf("fs.search: path must be absolute")
	}

	if v, ok := args["pattern"]; ok {
		_ = json.Unmarshal(v, &pattern)
	}
	if pattern == "" {
		pattern = "*"
	}
	pattern = strings.TrimSpace(pattern)
	if _, err := filepath.Match(pattern, "probe.txt"); err != nil {
		return nil, fmt.Errorf("fs.search: bad pattern: %w", err)
	}

	if v, ok := args["max_results"]; ok {
		_ = json.Unmarshal(v, &maxResults)
	}
	if maxResults <= 0 {
		maxResults = 200
	}
	if maxResults > 2000 {
		maxResults = 2000
	}

	if v, ok := args["max_depth"]; ok {
		_ = json.Unmarshal(v, &maxDepth)
	}
	if maxDepth < 0 {
		maxDepth = 0
	}
	if maxDepth > 20 {
		maxDepth = 20
	}

	if v, ok := args["include_dirs"]; ok {
		_ = json.Unmarshal(v, &includeDirs)
	}
	if v, ok := args["include_hidden"]; ok {
		_ = json.Unmarshal(v, &includeHidden)
	}

	rootInfo, err := os.Stat(searchPath)
	if err != nil {
		return nil, err
	}
	if !rootInfo.IsDir() {
		return nil, fmt.Errorf("fs.search: %q is not a directory", searchPath)
	}

	lowerPattern := strings.ToLower(pattern)
	entries := make([]map[string]interface{}, 0, maxResults)
	rootDepth := len(strings.Split(filepath.ToSlash(filepath.Clean(searchPath)), "/"))

	walkErr := filepath.WalkDir(searchPath, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		name := d.Name()
		if name == "." || name == "" {
			return nil
		}
		isDir := d.IsDir()

		if isHiddenPath(name) && !includeHidden {
			if isDir {
				return filepath.SkipDir
			}
			return nil
		}

		depth := len(strings.Split(filepath.ToSlash(path), "/")) - rootDepth
		if depth > maxDepth {
			if isDir {
				return filepath.SkipDir
			}
			return nil
		}
		if depth == 0 && !includeDirs {
			return nil
		}

		matches, matchErr := filepath.Match(lowerPattern, strings.ToLower(name))
		if matchErr != nil {
			return nil
		}
		if !matches {
			return nil
		}

		info, infoErr := d.Info()
		if infoErr != nil {
			return nil
		}

		size := int64(0)
		if !isDir {
			size = info.Size()
		}
		entries = append(entries, map[string]interface{}{
			"path":  path,
			"name":  name,
			"dir":   isDir,
			"size":  size,
			"mtime": info.ModTime().UTC().Format("2006-01-02T15:04:05Z"),
		})

		if len(entries) >= maxResults {
			return errors.New("fs.search: result limit reached")
		}
		return nil
	})
	if walkErr != nil && walkErr.Error() != "fs.search: result limit reached" {
		return nil, walkErr
	}

	return map[string]interface{}{
		"path":          searchPath,
		"pattern":       pattern,
		"max_depth":     maxDepth,
		"max_results":   maxResults,
		"include_dirs":  includeDirs,
		"include_hidden": includeHidden,
		"count":         len(entries),
		"entries":       entries,
	}, nil
}

func isHiddenPath(name string) bool {
	return strings.HasPrefix(name, ".") && len(name) > 1
}

const fsTextSampleCap = 256 * 1024

func fsHead(_ context.Context, args map[string]json.RawMessage) (interface{}, error) {
	return fsTextSample(args, false)
}

func fsTail(_ context.Context, args map[string]json.RawMessage) (interface{}, error) {
	return fsTextSample(args, true)
}

func fsTextSample(args map[string]json.RawMessage, tail bool) (interface{}, error) {
	var path string
	lines := 80
	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &path)
	}
	if v, ok := args["lines"]; ok {
		_ = json.Unmarshal(v, &lines)
	}
	path = strings.TrimSpace(path)
	if path == "" {
		return nil, fmt.Errorf("fs.%s: 'path' is required", sampleName(tail))
	}
	path = filepath.Clean(path)
	if !filepath.IsAbs(path) {
		return nil, fmt.Errorf("fs.%s: path must be absolute", sampleName(tail))
	}
	if lines <= 0 {
		lines = 80
	}
	if lines > 5000 {
		lines = 5000
	}

	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	if info.IsDir() {
		return nil, fmt.Errorf("fs.%s: %q is a directory", sampleName(tail), path)
	}
	if info.Size() > fsTextSampleCap*4 && !tail {
		// fs.head only reads from the start; still cap the read to keep memory bounded.
	}

	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	var data []byte
	if tail {
		// Read the trailing window. We over-read a bit so the last partial line is recoverable.
		readSize := int64(fsTextSampleCap)
		if info.Size() < readSize {
			readSize = info.Size()
		}
		if _, err := f.Seek(-readSize, io.SeekEnd); err != nil {
			return nil, err
		}
		data, err = io.ReadAll(io.LimitReader(f, readSize))
		if err != nil {
			return nil, err
		}
		if info.Size() > readSize {
			// Drop the leading partial line so we always start on a line boundary.
			if i := bytesIndexByte(data, '\n'); i >= 0 && i < len(data)-1 {
				data = data[i+1:]
			}
		}
	} else {
		data, err = io.ReadAll(io.LimitReader(f, fsTextSampleCap))
		if err != nil {
			return nil, err
		}
	}

	text := string(data)
	allLines := strings.Split(strings.TrimRight(text, "\n"), "\n")
	if tail {
		if len(allLines) > lines {
			allLines = allLines[len(allLines)-lines:]
		}
	} else {
		if len(allLines) > lines {
			allLines = allLines[:lines]
		}
	}

	return map[string]interface{}{
		"path":      path,
		"size":      info.Size(),
		"mtime":     info.ModTime().UTC().Format("2006-01-02T15:04:05Z"),
		"lines":     allLines,
		"line_count": len(allLines),
		"truncated": info.Size() > int64(len(data)),
		"mode":      sampleName(tail),
	}, nil
}

func sampleName(tail bool) string {
	if tail {
		return "tail"
	}
	return "head"
}

func bytesIndexByte(b []byte, c byte) int {
	for i, x := range b {
		if x == c {
			return i
		}
	}
	return -1
}

func fsStat(_ context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var path string
	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &path)
	}
	path = strings.TrimSpace(path)
	if path == "" {
		return nil, fmt.Errorf("fs.stat: 'path' is required")
	}
	path = filepath.Clean(path)
	if !filepath.IsAbs(path) {
		return nil, fmt.Errorf("fs.stat: path must be absolute")
	}

	info, err := os.Lstat(path)
	if err != nil {
		return nil, err
	}
	abs, _ := filepath.Abs(path)
	target := ""
	if info.Mode()&os.ModeSymlink != 0 {
		if dest, err := os.Readlink(path); err == nil {
			target = dest
		}
	}
	return map[string]interface{}{
		"path":          abs,
		"name":          filepath.Base(path),
		"size":          info.Size(),
		"mtime":         info.ModTime().UTC().Format("2006-01-02T15:04:05Z"),
		"mode":          info.Mode().String(),
		"is_dir":        info.IsDir(),
		"is_symlink":    info.Mode()&os.ModeSymlink != 0,
		"symlink_target": target,
	}, nil
}

func fsTree(_ context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var path string
	if v, ok := args["path"]; ok {
		_ = json.Unmarshal(v, &path)
	}
	path = strings.TrimSpace(path)
	if path == "" {
		return nil, fmt.Errorf("fs.tree: 'path' is required")
	}
	path = filepath.Clean(path)
	if !filepath.IsAbs(path) {
		return nil, fmt.Errorf("fs.tree: path must be absolute")
	}

	maxDepth := 3
	if v, ok := args["max_depth"]; ok {
		_ = json.Unmarshal(v, &maxDepth)
	}
	if maxDepth < 1 {
		maxDepth = 1
	}
	if maxDepth > 8 {
		maxDepth = 8
	}

	maxEntries := 500
	if v, ok := args["max_entries"]; ok {
		_ = json.Unmarshal(v, &maxEntries)
	}
	if maxEntries < 50 {
		maxEntries = 50
	}
	if maxEntries > 5000 {
		maxEntries = 5000
	}

	rootInfo, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	if !rootInfo.IsDir() {
		return nil, fmt.Errorf("fs.tree: %q is not a directory", path)
	}

	rootDepth := len(strings.Split(filepath.ToSlash(filepath.Clean(path)), "/"))
	entries := make([]map[string]interface{}, 0, maxEntries)

	walkErr := filepath.WalkDir(path, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if p == path {
			return nil
		}
		depth := len(strings.Split(filepath.ToSlash(p), "/")) - rootDepth
		if depth > maxDepth {
			if d.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		info, infoErr := d.Info()
		if infoErr != nil {
			return nil
		}
		entry := map[string]interface{}{
			"path":  p,
			"name":  d.Name(),
			"depth": depth,
			"dir":   d.IsDir(),
			"size":  info.Size(),
		}
		entries = append(entries, entry)
		if len(entries) >= maxEntries {
			return errors.New("fs.tree: entry cap reached")
		}
		return nil
	})
	if walkErr != nil && walkErr.Error() != "fs.tree: entry cap reached" {
		return nil, walkErr
	}

	return map[string]interface{}{
		"path":        path,
		"max_depth":   maxDepth,
		"max_entries": maxEntries,
		"count":       len(entries),
		"truncated":   len(entries) >= maxEntries,
		"entries":     entries,
	}, nil
}

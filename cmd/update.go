// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// Update implements `handoff update`. Checks the relay's published
// version manifest at <relay>/dl/handoff-version.json and, if a newer
// release is available, downloads it next to the running binary as
// handoff.exe.new. The user replaces the old binary manually -- we
// don't try to atomic-swap a running executable on Windows.
func Update(args []string) {
	check := false
	for _, a := range args {
		if a == "--check" || a == "-c" {
			check = true
		}
	}

	relay := strings.TrimRight(defaultRelay(), "/")
	manifestURL := relay + "/dl/handoff-version.json"
	exeURL := relay + "/dl/handoff.exe"

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	current := getCurrentVersion()

	fmt.Println("current:", current)
	fmt.Println("checking:", manifestURL)

	man, err := fetchManifest(ctx, manifestURL)
	if err != nil {
		fmt.Fprintln(os.Stderr, "could not reach update manifest:", err)
		os.Exit(1)
	}
	fmt.Println("latest: ", man.Version)
	if man.Notes != "" {
		fmt.Println("notes:  ", man.Notes)
	}

	if man.Version == current {
		fmt.Println("up to date")
		return
	}
	if check {
		fmt.Println("(check-only; not downloading)")
		return
	}

	exePath, err := os.Executable()
	if err != nil {
		fmt.Fprintln(os.Stderr, "could not resolve current executable:", err)
		os.Exit(1)
	}
	dst := filepath.Join(filepath.Dir(exePath), "handoff.exe.new")

	fmt.Println("downloading:", exeURL, "->", dst)
	if err := downloadVerified(ctx, exeURL, man.Sha256, dst); err != nil {
		fmt.Fprintln(os.Stderr, "download failed:", err)
		_ = os.Remove(dst)
		os.Exit(1)
	}
	fmt.Println("downloaded.")
	fmt.Println()
	fmt.Println("To finish the update:")
	fmt.Println("  1) Close any running 'handoff' processes.")
	fmt.Println("  2) Replace", exePath)
	fmt.Println("     with    ", dst)
	fmt.Println("  3) Start a new session.")
}

type versionManifest struct {
	Version string `json:"version"`
	Sha256  string `json:"sha256"`
	Notes   string `json:"notes"`
	URL     string `json:"url"`
}

func fetchManifest(ctx context.Context, url string) (*versionManifest, error) {
	req, _ := http.NewRequestWithContext(ctx, "GET", url, nil)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("manifest fetch: %d", resp.StatusCode)
	}
	var m versionManifest
	if err := json.NewDecoder(resp.Body).Decode(&m); err != nil {
		return nil, err
	}
	if m.Version == "" {
		return nil, fmt.Errorf("manifest has empty version")
	}
	return &m, nil
}

func downloadVerified(ctx context.Context, url, sha256Hex, dst string) error {
	req, _ := http.NewRequestWithContext(ctx, "GET", url, nil)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return fmt.Errorf("%d", resp.StatusCode)
	}

	f, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o755)
	if err != nil {
		return err
	}
	h := sha256.New()
	written, err := io.Copy(io.MultiWriter(f, h), resp.Body)
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		return err
	}
	if written == 0 {
		return fmt.Errorf("empty body")
	}
	if sha256Hex != "" {
		got := hex.EncodeToString(h.Sum(nil))
		if !strings.EqualFold(got, sha256Hex) {
			return fmt.Errorf("sha256 mismatch (got %s, expected %s)", got, sha256Hex)
		}
	}
	return nil
}

// getCurrentVersion returns the version string baked into the binary.
// We re-read from main's exported const via an env-injected fallback;
// in v0.1 we just print "0.1.x" so the operator has something to
// compare with the manifest until the build stamps a real version.
func getCurrentVersion() string {
	if v := os.Getenv("HANDOFF_VERSION"); v != "" {
		return v
	}
	return Version
}

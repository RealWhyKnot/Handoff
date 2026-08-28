// SPDX-License-Identifier: GPL-3.0-or-later
package cmd

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
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
	var check, force, noReplace bool
	opts, _, err := parseOptions("update", args, func(fs *flag.FlagSet) {
		fs.BoolVar(&check, "check", false, "report what is available without downloading")
		fs.BoolVar(&check, "c", false, "report what is available without downloading")
		fs.BoolVar(&force, "force", false, "update even from an unversioned local build")
		fs.BoolVar(&noReplace, "no-replace", false, "download only, leaving the current binary in place")
	})
	if err != nil {
		if !errors.Is(err, flag.ErrHelp) {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(2)
		}
		return
	}

	relay := opts.Relay
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

	if !shouldUpdate(current, man.Version, force) {
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

	if noReplace {
		fmt.Println()
		fmt.Println("To finish the update:")
		fmt.Println("  1) Close any running 'handoff' processes.")
		fmt.Println("  2) Replace", exePath)
		fmt.Println("     with    ", dst)
		fmt.Println("  3) Start a new session.")
		return
	}

	if err := replaceExecutable(exePath, dst); err != nil {
		fmt.Fprintln(os.Stderr, "could not install the update:", err)
		fmt.Fprintln(os.Stderr, "the new build is at", dst)
		os.Exit(1)
	}
	fmt.Println("updated. Restart handoff to use", man.Version)
}

// shouldUpdate compares versions numerically. String equality used to mean a
// local build always looked out of date, and an older published build looked
// newer.
func shouldUpdate(current, latest string, force bool) bool {
	cur, curOK := parseVersion(current)
	next, nextOK := parseVersion(latest)

	if !nextOK {
		fmt.Println("the relay published an unreadable version; not updating")
		return false
	}
	if !curOK {
		if !force {
			fmt.Println("this is a development build; not updating (use --force)")
			return false
		}
		return true
	}

	switch compareVersions(cur, next) {
	case 0:
		fmt.Println("up to date")
		return false
	case 1:
		fmt.Printf("already newer than the published build (%s > %s)\n", current, latest)
		return false
	default:
		return true
	}
}

// replaceExecutable swaps in the downloaded build. Windows will not let a
// running image be overwritten, but it will let it be renamed, so the old one
// is moved aside and swept on a later start.
func replaceExecutable(exePath, staged string) error {
	old := exePath + ".old"
	_ = os.Remove(old)

	if err := os.Rename(exePath, old); err != nil {
		return fmt.Errorf("move the current binary aside: %w", err)
	}
	if err := os.Rename(staged, exePath); err != nil {
		if restoreErr := os.Rename(old, exePath); restoreErr != nil {
			return fmt.Errorf("install failed (%w) and the original could not be restored from %s: %v", err, old, restoreErr)
		}
		return fmt.Errorf("install the new binary: %w", err)
	}
	// Expected to fail while this process is still running the old image.
	_ = os.Remove(old)
	return nil
}

// SweepStaleUpdate removes the previous binary left behind by an update. It is
// best effort: the file is locked until the process using it exits.
func SweepStaleUpdate() {
	exe, err := os.Executable()
	if err != nil {
		return
	}
	_ = os.Remove(exe + ".old")
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

// getCurrentVersion returns the version stamped into this binary at build
// time. It is deliberately not overridable at runtime: HANDOFF_VERSION is a
// build variable, and honouring it here made an exported shell value silently
// change what the updater believed was installed.
func getCurrentVersion() string {
	return Version
}

// SPDX-License-Identifier: GPL-3.0-or-later
//go:build windows

package capabilities

import (
	"context"
	"strings"
	"testing"
)

// Screen capture is one of the few things that cannot be proved with a stub:
// Windows antivirus inspects the script through AMSI and will refuse patterns
// that look like an infostealer, which no unit test with a fake capture
// function would ever notice.
func TestRealScreenCaptureIsNotBlockedAndProducesAJpeg(t *testing.T) {
	shot, err := captureScreen(context.Background(), screenshotOptions{
		Display: 0, Format: "jpeg", Quality: 70, MaxWidth: 800,
	})
	if err != nil {
		if strings.Contains(err.Error(), "malicious") {
			t.Fatalf("the capture script was blocked by antivirus: %v", err)
		}
		t.Skipf("no capturable display in this environment: %v", err)
	}

	if len(shot.Data) < 1000 {
		t.Fatalf("image is only %d bytes", len(shot.Data))
	}
	if shot.Data[0] != 0xFF || shot.Data[1] != 0xD8 {
		t.Fatalf("not a JPEG: % x", shot.Data[:4])
	}
	if shot.Width != 800 {
		t.Fatalf("width = %d, want the requested 800", shot.Width)
	}
	if !shot.Scaled {
		t.Fatal("a 800px capture of a larger screen should report scaled")
	}
}

func TestRealScreenCapturePng(t *testing.T) {
	shot, err := captureScreen(context.Background(), screenshotOptions{
		Display: 0, Format: "png", Quality: 70, MaxWidth: 640,
	})
	if err != nil {
		if strings.Contains(err.Error(), "malicious") {
			t.Fatalf("the capture script was blocked by antivirus: %v", err)
		}
		t.Skipf("no capturable display in this environment: %v", err)
	}
	if string(shot.Data[1:4]) != "PNG" {
		t.Fatalf("not a PNG: % x", shot.Data[:8])
	}
}

func TestParseShotDimensions(t *testing.T) {
	w, h, scaled, ok := parseShotDimensions("1280x720x1")
	if !ok || w != 1280 || h != 720 || !scaled {
		t.Fatalf("got %d %d %v %v", w, h, scaled, ok)
	}
	if _, _, _, ok := parseShotDimensions("garbage"); ok {
		t.Fatal("garbage was parsed as dimensions")
	}
}

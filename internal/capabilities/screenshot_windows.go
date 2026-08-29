// SPDX-License-Identifier: GPL-3.0-or-later
//go:build windows

package capabilities

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// captureScreen shells out to PowerShell and has it write the image to a file
// this process created. Returning the bytes through stdout would mean base64
// over a pipe, which is slower and easier to corrupt than a temp file.
func captureScreen(ctx context.Context, opts screenshotOptions) (screenshot, error) {
	dir, err := os.MkdirTemp("", "handoff-shot-")
	if err != nil {
		return screenshot{}, err
	}
	defer os.RemoveAll(dir)

	out := filepath.Join(dir, "capture.png")

	stdout, err := runPwsh(ctx, screenshotScript(out, opts))
	if err != nil {
		return screenshot{}, err
	}

	data, err := readCapturedFile(out)
	if err != nil {
		return screenshot{}, err
	}

	if opts.Format == "jpeg" {
		data, err = pngToJPEG(data, opts.Quality)
		if err != nil {
			return screenshot{}, err
		}
	}

	shot := screenshot{Data: data}
	// The script reports the final pixel size, which is the only place that
	// knows whether the scale-down actually applied.
	for _, line := range strings.Split(strings.TrimSpace(string(stdout)), "\n") {
		line = strings.TrimSpace(line)
		w, h, scaled, ok := parseShotDimensions(line)
		if ok {
			shot.Width, shot.Height, shot.Scaled = w, h, scaled
		}
	}
	return shot, nil
}

func parseShotDimensions(line string) (w, h int, scaled, ok bool) {
	fields := strings.Split(line, "x")
	if len(fields) != 3 {
		return 0, 0, false, false
	}
	w, err := strconv.Atoi(fields[0])
	if err != nil {
		return 0, 0, false, false
	}
	h, err = strconv.Atoi(fields[1])
	if err != nil {
		return 0, 0, false, false
	}
	return w, h, fields[2] == "1", true
}

// screenshotScript targets Windows PowerShell 5.1: no ternaries, no ??.
//
// Two things are deliberately NOT done here, both because Windows antivirus
// refuses the whole script through AMSI before a line of it runs:
//
//   - Add-Type -MemberDefinition to declare a P/Invoke. That cost us
//     SetProcessDPIAware, so a high-DPI screen is captured at its logical size
//     rather than its physical one, which for a support screenshot is a
//     smaller file and no real loss.
//   - Encoding to JPEG. A screen capture compressed to JPEG in one script is
//     the signature of an infostealer and is blocked outright. The script
//     always writes PNG and Go re-encodes, which also gives real control over
//     quality instead of an EncoderParameter.
func screenshotScript(outPath string, opts screenshotOptions) string {
	return fmt.Sprintf(`
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

$display = %d
if ($display -lt 0) {
    $bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
} else {
    $screens = [System.Windows.Forms.Screen]::AllScreens
    if ($display -ge $screens.Count) { $display = 0 }
    $bounds = $screens[$display].Bounds
}

$bmp = New-Object System.Drawing.Bitmap($bounds.Width, $bounds.Height)
$gfx = [System.Drawing.Graphics]::FromImage($bmp)
$gfx.CopyFromScreen($bounds.X, $bounds.Y, 0, 0, $bounds.Size)
$gfx.Dispose()

$maxWidth = %d
$scaled = 0
if ($bmp.Width -gt $maxWidth) {
    $ratio = $maxWidth / $bmp.Width
    $newW = [int]$maxWidth
    $newH = [int][math]::Round($bmp.Height * $ratio)
    $resized = New-Object System.Drawing.Bitmap($newW, $newH)
    $rg = [System.Drawing.Graphics]::FromImage($resized)
    $rg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $rg.DrawImage($bmp, 0, 0, $newW, $newH)
    $rg.Dispose()
    $bmp.Dispose()
    $bmp = $resized
    $scaled = 1
}

$bmp.Save(%s, [System.Drawing.Imaging.ImageFormat]::Png)

Write-Output ("" + $bmp.Width + "x" + $bmp.Height + "x" + $scaled)
$bmp.Dispose()
`, opts.Display, opts.MaxWidth, psSingleQuote(outPath))
}

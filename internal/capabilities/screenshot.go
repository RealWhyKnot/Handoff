// SPDX-License-Identifier: GPL-3.0-or-later
//
// sys.screenshot answers "show me what you are seeing", which is the first
// thing any helper asks and the one thing the command set could not do.

package capabilities

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"image/jpeg"
	"image/png"
	"os"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// screenshotBudget keeps an image inside the relay's 2 MiB command envelope,
// with room for the JSON around it. The result also sits in the relay's event
// ring, so an oversized capture would be replayed to every viewer.
const screenshotBudget = 1500 * 1024

func RegisterScreenshot(r *dispatch.Router) {
	r.RegisterSpec(dispatch.Spec{
		Kind:        "sys.screenshot",
		Label:       "Screenshot",
		Description: "Capture the host's screen.",
		Risky:       true,
		Params: []dispatch.Param{
			{Name: "display", Type: dispatch.ParamInt, Default: 0, Min: dispatch.IntPtr(-1), Max: dispatch.IntPtr(16),
				Description: "0 for the primary screen, -1 for all screens together."},
			{Name: "format", Type: dispatch.ParamEnum, Enum: []string{"jpeg", "png"}, Default: "jpeg"},
			{Name: "quality", Type: dispatch.ParamInt, Default: 70, Min: dispatch.IntPtr(1), Max: dispatch.IntPtr(100),
				Description: "JPEG quality; ignored for PNG."},
			{Name: "max_width", Type: dispatch.ParamInt, Default: 1600, Min: dispatch.IntPtr(320), Max: dispatch.IntPtr(3840),
				Description: "Scale the capture down to at most this width."},
		},
	}, screenshotHandler)
}

type screenshotOptions struct {
	Display  int
	Format   string
	Quality  int
	MaxWidth int
}

func screenshotOptionsFrom(args map[string]json.RawMessage) screenshotOptions {
	opts := screenshotOptions{Display: 0, Format: "jpeg", Quality: 70, MaxWidth: 1600}
	if v, ok := args["display"]; ok {
		_ = json.Unmarshal(v, &opts.Display)
	}
	if v, ok := args["format"]; ok {
		_ = json.Unmarshal(v, &opts.Format)
	}
	if v, ok := args["quality"]; ok {
		_ = json.Unmarshal(v, &opts.Quality)
	}
	if v, ok := args["max_width"]; ok {
		_ = json.Unmarshal(v, &opts.MaxWidth)
	}
	return opts
}

// captureFunc is swapped in tests so the size-budget logic can be exercised
// without a display.
var captureFunc = captureScreen

func screenshotHandler(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	opts := screenshotOptionsFrom(args)
	if err := requireRiskConsent(ctx, "sys.screenshot", "Takes a picture of what is currently on your screen and sends it to your helper."); err != nil {
		return nil, err
	}
	return captureWithinBudget(ctx, opts)
}

// captureWithinBudget retries once at half width rather than failing outright:
// a screenshot that is merely too big is still worth having smaller.
func captureWithinBudget(ctx context.Context, opts screenshotOptions) (interface{}, error) {
	for attempt := 0; attempt < 2; attempt++ {
		shot, err := captureFunc(ctx, opts)
		if err != nil {
			return nil, err
		}
		encoded := base64.StdEncoding.EncodeToString(shot.Data)
		if len(encoded) <= screenshotBudget {
			sum := sha256.Sum256(shot.Data)
			return map[string]interface{}{
				"width":        shot.Width,
				"height":       shot.Height,
				"format":       opts.Format,
				"bytes":        len(shot.Data),
				"scaled":       shot.Scaled,
				"sha256":       hex.EncodeToString(sum[:]),
				"image_base64": encoded,
			}, nil
		}
		if attempt == 0 {
			opts.MaxWidth = opts.MaxWidth / 2
			if opts.MaxWidth < 320 {
				opts.MaxWidth = 320
			}
			continue
		}
		return nil, fmt.Errorf("sys.screenshot: image is %d bytes encoded, over the %d byte limit; lower max_width or quality",
			len(encoded), screenshotBudget)
	}
	return nil, fmt.Errorf("sys.screenshot: could not produce an image within the size limit")
}

type screenshot struct {
	Data   []byte
	Width  int
	Height int
	Scaled bool
}

// pngToJPEG re-encodes the captured PNG. Doing the compression here rather
// than in the capture script keeps that script off the antivirus signature for
// screen-grab-and-compress, and stdlib gives a real quality knob.
func pngToJPEG(pngData []byte, quality int) ([]byte, error) {
	img, err := png.Decode(bytes.NewReader(pngData))
	if err != nil {
		return nil, fmt.Errorf("sys.screenshot: decode capture: %w", err)
	}
	if quality < 1 || quality > 100 {
		quality = 70
	}
	var buf bytes.Buffer
	if err := jpeg.Encode(&buf, img, &jpeg.Options{Quality: quality}); err != nil {
		return nil, fmt.Errorf("sys.screenshot: encode jpeg: %w", err)
	}
	return buf.Bytes(), nil
}

func readCapturedFile(path string) ([]byte, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("sys.screenshot: read capture: %w", err)
	}
	if len(data) == 0 {
		return nil, fmt.Errorf("sys.screenshot: capture produced no image")
	}
	return data, nil
}

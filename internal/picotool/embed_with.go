// SPDX-License-Identifier: GPL-3.0-or-later
//go:build embed_picotool

package picotool

import _ "embed"

// data carries the bundled picotool.exe. Populated only on builds
// invoked with `-tags embed_picotool`; default builds compile in the
// empty stub from embed_without.go and fall back to the host PATH.
//
//go:embed binaries/picotool.exe
var data []byte

// Embedded reports whether the binary was baked into this build.
func Embedded() bool { return len(data) > 0 }

// SPDX-License-Identifier: GPL-3.0-or-later
//go:build !embed_picotool

package picotool

// data is empty on default builds; pico.* handlers fall back to PATH.
var data []byte

// Embedded reports whether the binary was baked into this build.
func Embedded() bool { return false }

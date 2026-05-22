// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"reflect"
	"strings"
	"testing"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

func TestWithSerialOnlyAppendsWhenNonEmpty(t *testing.T) {
	got := withSerial([]string{"info", "-a"}, "")
	want := []string{"info", "-a"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("withSerial empty = %#v, want %#v", got, want)
	}

	got = withSerial([]string{"info", "-a"}, "E660583883278628")
	want = []string{"info", "-a", "--id", "E660583883278628"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("withSerial = %#v, want %#v", got, want)
	}
}

func TestPicoStringArgTrimsAndDefaults(t *testing.T) {
	args := rawArgs(t, map[string]interface{}{
		"present":   "  abc  ",
		"empty":     "",
		"only_ws":   "   ",
		"non_str":   42,
	})
	if got := picoStringArg(args, "present"); got != "abc" {
		t.Fatalf("present = %q, want abc", got)
	}
	if got := picoStringArg(args, "missing"); got != "" {
		t.Fatalf("missing = %q, want empty", got)
	}
	if got := picoStringArg(args, "empty"); got != "" {
		t.Fatalf("empty = %q, want empty", got)
	}
	if got := picoStringArg(args, "only_ws"); got != "" {
		t.Fatalf("only_ws = %q, want empty", got)
	}
	if got := picoStringArg(args, "non_str"); got != "" {
		t.Fatalf("non_str = %q, want empty", got)
	}
}

func TestPicoBootselArgsDefaults(t *testing.T) {
	got, err := picoBootselArgs(rawArgs(t, map[string]interface{}{}))
	if err != nil {
		t.Fatalf("picoBootselArgs err = %v", err)
	}
	want := []string{"reboot", "-f", "-u"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("picoBootselArgs = %#v, want %#v", got, want)
	}
}

func TestPicoBootselArgsWithCPUPartitionAndSerial(t *testing.T) {
	got, err := picoBootselArgs(rawArgs(t, map[string]interface{}{
		"cpu":       "riscv",
		"partition": "boot-a",
		"serial":    "ABCD1234",
	}))
	if err != nil {
		t.Fatalf("picoBootselArgs err = %v", err)
	}
	want := []string{"reboot", "-f", "-u", "-c", "riscv", "-g", "boot-a", "--id", "ABCD1234"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("picoBootselArgs = %#v, want %#v", got, want)
	}
}

func TestPicoBootselArgsRejectsUnknownCPU(t *testing.T) {
	_, err := picoBootselArgs(rawArgs(t, map[string]interface{}{
		"cpu": "x86",
	}))
	if err == nil || !strings.Contains(err.Error(), "cpu must be") {
		t.Fatalf("picoBootselArgs err = %v, want cpu validation", err)
	}
}

func TestPicoResetArgsDefaults(t *testing.T) {
	got, err := picoResetArgs(rawArgs(t, map[string]interface{}{}))
	if err != nil {
		t.Fatalf("picoResetArgs err = %v", err)
	}
	want := []string{"reboot"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("picoResetArgs = %#v, want %#v", got, want)
	}
}

func TestPicoResetArgsAppendsCPUPartitionAndSerial(t *testing.T) {
	got, err := picoResetArgs(rawArgs(t, map[string]interface{}{
		"cpu":       "arm",
		"partition": "0",
		"serial":    "DEADBEEF",
	}))
	if err != nil {
		t.Fatalf("picoResetArgs err = %v", err)
	}
	want := []string{"reboot", "-c", "arm", "-g", "0", "--id", "DEADBEEF"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("picoResetArgs = %#v, want %#v", got, want)
	}
}

func TestPicoResetArgsRejectsUnknownCPU(t *testing.T) {
	_, err := picoResetArgs(rawArgs(t, map[string]interface{}{
		"cpu": "mips",
	}))
	if err == nil || !strings.Contains(err.Error(), "cpu must be") {
		t.Fatalf("picoResetArgs err = %v, want cpu validation", err)
	}
}

func TestPicoFlashArgsRequiresUF2(t *testing.T) {
	_, _, err := picoFlashArgs(rawArgs(t, map[string]interface{}{}))
	if err == nil || !strings.Contains(err.Error(), "uf2_path") {
		t.Fatalf("picoFlashArgs err = %v, want uf2_path required", err)
	}
}

func TestPicoFlashArgsDefaults(t *testing.T) {
	got, uf2, err := picoFlashArgs(rawArgs(t, map[string]interface{}{
		"uf2_path": `C:\fw\app.uf2`,
	}))
	if err != nil {
		t.Fatalf("picoFlashArgs err = %v", err)
	}
	if uf2 != `C:\fw\app.uf2` {
		t.Fatalf("uf2 = %q", uf2)
	}
	want := []string{"load", "-fx", `C:\fw\app.uf2`}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("picoFlashArgs = %#v, want %#v", got, want)
	}
}

func TestPicoFlashArgsAppendsFamilyAndSerial(t *testing.T) {
	got, _, err := picoFlashArgs(rawArgs(t, map[string]interface{}{
		"uf2_path": `C:\fw\app.uf2`,
		"family":   "rp2350-arm-s",
		"serial":   "E0C912D24340",
	}))
	if err != nil {
		t.Fatalf("picoFlashArgs err = %v", err)
	}
	want := []string{"load", "-fx", "--family", "rp2350-arm-s", `C:\fw\app.uf2`, "--id", "E0C912D24340"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("picoFlashArgs = %#v, want %#v", got, want)
	}
}

func TestPicoFlashArgsRejectsUnknownFamily(t *testing.T) {
	_, _, err := picoFlashArgs(rawArgs(t, map[string]interface{}{
		"uf2_path": `C:\fw\app.uf2`,
		"family":   "rp9999",
	}))
	if err == nil || !strings.Contains(err.Error(), "unknown family") {
		t.Fatalf("picoFlashArgs err = %v, want family validation", err)
	}
}

func TestPicoVerifyArgsRequiresFilePath(t *testing.T) {
	_, _, err := picoVerifyArgs(rawArgs(t, map[string]interface{}{}))
	if err == nil || !strings.Contains(err.Error(), "file_path") {
		t.Fatalf("picoVerifyArgs err = %v, want file_path required", err)
	}
}

func TestPicoVerifyArgsDefaults(t *testing.T) {
	got, path, err := picoVerifyArgs(rawArgs(t, map[string]interface{}{
		"file_path": `C:\fw\app.uf2`,
	}))
	if err != nil {
		t.Fatalf("picoVerifyArgs err = %v", err)
	}
	if path != `C:\fw\app.uf2` {
		t.Fatalf("path = %q", path)
	}
	want := []string{"verify", "-f", `C:\fw\app.uf2`}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("picoVerifyArgs = %#v, want %#v", got, want)
	}
}

func TestPicoVerifyArgsAppendsFamilyAndSerial(t *testing.T) {
	got, _, err := picoVerifyArgs(rawArgs(t, map[string]interface{}{
		"file_path": `C:\fw\app.uf2`,
		"family":    "rp2040",
		"serial":    "EEEEEEEEEEEEEEEE",
	}))
	if err != nil {
		t.Fatalf("picoVerifyArgs err = %v", err)
	}
	want := []string{"verify", "-f", "--family", "rp2040", `C:\fw\app.uf2`, "--id", "EEEEEEEEEEEEEEEE"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("picoVerifyArgs = %#v, want %#v", got, want)
	}
}

func TestPicoVerifyArgsRejectsUnknownFamily(t *testing.T) {
	_, _, err := picoVerifyArgs(rawArgs(t, map[string]interface{}{
		"file_path": `C:\fw\app.uf2`,
		"family":    "abcd",
	}))
	if err == nil || !strings.Contains(err.Error(), "unknown family") {
		t.Fatalf("picoVerifyArgs err = %v, want family validation", err)
	}
}

func TestRegisterPicoRegistersAllKinds(t *testing.T) {
	r := dispatch.New()
	RegisterPico(r)
	got := r.Kinds()
	want := []string{
		"pico.bootsel",
		"pico.flash",
		"pico.info",
		"pico.list",
		"pico.reset",
		"pico.save",
		"pico.verify",
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("RegisterPico kinds = %#v, want %#v", got, want)
	}
}

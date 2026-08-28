// SPDX-License-Identifier: GPL-3.0-or-later

package dispatch

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
)

func rawArgs(t *testing.T, kv map[string]interface{}) map[string]json.RawMessage {
	t.Helper()
	out := map[string]json.RawMessage{}
	for k, v := range kv {
		enc, err := json.Marshal(v)
		if err != nil {
			t.Fatalf("marshal %s: %v", k, err)
		}
		out[k] = enc
	}
	return out
}

func listSpec() Spec {
	return Spec{
		Kind: "app.list",
		Params: []Param{
			{Name: "limit", Type: ParamInt, Default: 300, Min: IntPtr(1), Max: IntPtr(5000), Aliases: []string{"max_results"}},
			{Name: "match", Type: ParamString},
			{Name: "match_mode", Type: ParamEnum, Enum: []string{"contains", "prefix", "regex"}, Default: "contains"},
			{Name: "include_hidden", Type: ParamBool, Default: false},
		},
	}
}

func TestBindInjectsDefaults(t *testing.T) {
	bound, clamped, ignored, err := listSpec().Bind(rawArgs(t, map[string]interface{}{}))
	if err != nil {
		t.Fatalf("bind: %v", err)
	}
	if len(clamped) != 0 || len(ignored) != 0 {
		t.Fatalf("expected no clamps or ignores, got %v / %v", clamped, ignored)
	}
	if got := string(bound["limit"]); got != "300" {
		t.Fatalf("limit default = %s, want 300", got)
	}
	if got := string(bound["match_mode"]); got != `"contains"` {
		t.Fatalf("match_mode default = %s", got)
	}
	if got := string(bound["include_hidden"]); got != "false" {
		t.Fatalf("include_hidden default = %s", got)
	}
}

func TestBindRejectsWrongType(t *testing.T) {
	_, _, _, err := listSpec().Bind(rawArgs(t, map[string]interface{}{"limit": "200"}))
	if err == nil {
		t.Fatal("expected a type error for a string limit")
	}
	if !strings.Contains(err.Error(), "must be a number") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestBindClampsAndReports(t *testing.T) {
	bound, clamped, _, err := listSpec().Bind(rawArgs(t, map[string]interface{}{"limit": 99999}))
	if err != nil {
		t.Fatalf("bind: %v", err)
	}
	if string(bound["limit"]) != "5000" {
		t.Fatalf("limit = %s, want 5000", bound["limit"])
	}
	if len(clamped) != 1 || clamped[0].Param != "limit" || clamped[0].From != 99999 || clamped[0].To != 5000 {
		t.Fatalf("unexpected clamp record: %+v", clamped)
	}
}

func TestBindResolvesAlias(t *testing.T) {
	bound, _, ignored, err := listSpec().Bind(rawArgs(t, map[string]interface{}{"max_results": 50}))
	if err != nil {
		t.Fatalf("bind: %v", err)
	}
	if string(bound["limit"]) != "50" {
		t.Fatalf("limit = %s, want 50 via alias", bound["limit"])
	}
	if _, still := bound["max_results"]; still {
		t.Fatal("alias key should be replaced by the canonical name")
	}
	if len(ignored) != 0 {
		t.Fatalf("alias must not be reported as ignored: %v", ignored)
	}
}

func TestBindPrefersCanonicalOverAlias(t *testing.T) {
	bound, _, _, err := listSpec().Bind(rawArgs(t, map[string]interface{}{"limit": 10, "max_results": 900}))
	if err != nil {
		t.Fatalf("bind: %v", err)
	}
	if string(bound["limit"]) != "10" {
		t.Fatalf("limit = %s, want the canonical 10", bound["limit"])
	}
}

func TestBindRejectsEnumMissAndNamesLegalValues(t *testing.T) {
	_, _, _, err := listSpec().Bind(rawArgs(t, map[string]interface{}{"match_mode": "fuzzy"}))
	if err == nil {
		t.Fatal("expected an enum error")
	}
	for _, want := range []string{"contains", "prefix", "regex"} {
		if !strings.Contains(err.Error(), want) {
			t.Fatalf("error should list %q: %v", want, err)
		}
	}
}

func TestBindReportsUnknownKeys(t *testing.T) {
	_, _, ignored, err := listSpec().Bind(rawArgs(t, map[string]interface{}{"maxresults": 5}))
	if err != nil {
		t.Fatalf("bind: %v", err)
	}
	if len(ignored) != 1 || ignored[0] != "maxresults" {
		t.Fatalf("expected maxresults reported as ignored, got %v", ignored)
	}
}

func TestBindRequiredMissing(t *testing.T) {
	spec := Spec{Kind: "fs.read", Params: []Param{{Name: "path", Type: ParamString, Required: true}}}
	_, _, _, err := spec.Bind(rawArgs(t, map[string]interface{}{}))
	if err == nil || !strings.Contains(err.Error(), "'path' is required") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestBindEnforcesMaxBytes(t *testing.T) {
	spec := Spec{Kind: "ps.exec", Params: []Param{{Name: "script", Type: ParamString, Required: true, MaxBytes: 8}}}
	_, _, _, err := spec.Bind(rawArgs(t, map[string]interface{}{"script": "way too long"}))
	if err == nil || !strings.Contains(err.Error(), "cap is 8") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestBindTreatsExplicitNullAsAbsent(t *testing.T) {
	bound, _, _, err := listSpec().Bind(map[string]json.RawMessage{"limit": json.RawMessage("null")})
	if err != nil {
		t.Fatalf("bind: %v", err)
	}
	if string(bound["limit"]) != "300" {
		t.Fatalf("null should fall back to the default, got %s", bound["limit"])
	}
}

func TestDispatchBindsBeforeHandler(t *testing.T) {
	r := New()
	var seen string
	r.RegisterSpec(listSpec(), func(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
		seen = string(args["limit"])
		return "ok", nil
	})

	out := r.Dispatch(context.Background(), "app.list", rawArgs(t, map[string]interface{}{"max_results": 7}))
	if !out.OK {
		t.Fatalf("dispatch failed: %s", out.Error)
	}
	if seen != "7" {
		t.Fatalf("handler saw limit=%s, want 7 resolved from the alias", seen)
	}
}

func TestDispatchSurfacesBindErrorWithoutCallingHandler(t *testing.T) {
	r := New()
	called := false
	r.RegisterSpec(listSpec(), func(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
		called = true
		return nil, nil
	})

	out := r.Dispatch(context.Background(), "app.list", rawArgs(t, map[string]interface{}{"limit": "nope"}))
	if out.OK {
		t.Fatal("expected a failed outcome")
	}
	if called {
		t.Fatal("handler must not run when binding fails")
	}
}

func TestDispatchCarriesClampsAndIgnores(t *testing.T) {
	r := New()
	r.RegisterSpec(listSpec(), func(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
		return "ok", nil
	})

	out := r.Dispatch(context.Background(), "app.list", rawArgs(t, map[string]interface{}{
		"limit": 99999,
		"typo":  1,
	}))
	if !out.OK {
		t.Fatalf("dispatch failed: %s", out.Error)
	}
	if len(out.Clamped) != 1 {
		t.Fatalf("expected one clamp, got %+v", out.Clamped)
	}
	if len(out.Ignored) != 1 || out.Ignored[0] != "typo" {
		t.Fatalf("expected typo ignored, got %v", out.Ignored)
	}
}

func TestDispatchUnpacksFailureDetail(t *testing.T) {
	r := New()
	code := 3
	r.Register("ps.exec", func(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
		return nil, &Failure{Message: "powershell exited 3", ExitCode: &code, Stderr: "boom"}
	})

	out := r.Dispatch(context.Background(), "ps.exec", nil)
	if out.OK {
		t.Fatal("a Failure must produce OK=false")
	}
	if out.Error != "powershell exited 3" {
		t.Fatalf("error = %q", out.Error)
	}
	detail, ok := out.Detail.(map[string]interface{})
	if !ok {
		t.Fatalf("detail = %#v", out.Detail)
	}
	if detail["exit_code"] != 3 {
		t.Fatalf("exit_code = %v", detail["exit_code"])
	}
	if detail["stderr"] != "boom" {
		t.Fatalf("stderr = %v", detail["stderr"])
	}
}

func TestRegisterWithoutSpecLeavesArgsUntouched(t *testing.T) {
	r := New()
	var got string
	r.Register("legacy.kind", func(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
		got = string(args["anything"])
		return nil, nil
	})

	out := r.Dispatch(context.Background(), "legacy.kind", rawArgs(t, map[string]interface{}{"anything": "kept"}))
	if !out.OK {
		t.Fatalf("dispatch failed: %s", out.Error)
	}
	if got != `"kept"` {
		t.Fatalf("unspecced handler saw %s", got)
	}
}

func TestSpecsCoverEveryRegisteredKind(t *testing.T) {
	r := New()
	r.Register("a.one", func(context.Context, map[string]json.RawMessage) (interface{}, error) { return nil, nil })
	r.RegisterSpec(listSpec(), func(context.Context, map[string]json.RawMessage) (interface{}, error) { return nil, nil })

	specs := r.Specs()
	if len(specs) != len(r.Kinds()) {
		t.Fatalf("specs (%d) must cover kinds (%d)", len(specs), len(r.Kinds()))
	}
	if specs[0].Kind != "a.one" || specs[1].Kind != "app.list" {
		t.Fatalf("specs are not in kind order: %+v", specs)
	}
}

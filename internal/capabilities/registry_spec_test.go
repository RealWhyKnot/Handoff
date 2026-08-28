// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

func specRouter(t *testing.T) *dispatch.Router {
	t.Helper()
	r := dispatch.New()
	RegisterAll(r, nil)
	return r
}

// The advertised schema is what operators and agents build requests from, so a
// spec that contradicts itself is worse than no spec at all.
func TestRegisteredSpecsAreInternallyConsistent(t *testing.T) {
	for _, spec := range specRouter(t).Specs() {
		if spec.Kind == "" {
			t.Fatal("a spec was registered with no kind")
		}
		seen := map[string]bool{}
		for _, p := range spec.Params {
			if p.Name == "" {
				t.Fatalf("%s: a param has no name", spec.Kind)
			}
			if seen[p.Name] {
				t.Fatalf("%s: duplicate param %q", spec.Kind, p.Name)
			}
			seen[p.Name] = true

			for _, alias := range p.Aliases {
				if seen[alias] {
					t.Fatalf("%s: alias %q collides with another param", spec.Kind, alias)
				}
				seen[alias] = true
			}

			if p.Type == dispatch.ParamEnum && len(p.Enum) == 0 {
				t.Fatalf("%s: param %q is an enum with no values", spec.Kind, p.Name)
			}
			if p.Required && p.Default != nil {
				t.Fatalf("%s: param %q is required and also has a default", spec.Kind, p.Name)
			}
			if p.Min != nil && p.Max != nil && *p.Min > *p.Max {
				t.Fatalf("%s: param %q has min above max", spec.Kind, p.Name)
			}
			if n, ok := p.Default.(int); ok {
				if p.Min != nil && n < *p.Min {
					t.Fatalf("%s: param %q default %d is below min", spec.Kind, p.Name, n)
				}
				if p.Max != nil && n > *p.Max {
					t.Fatalf("%s: param %q default %d is above max", spec.Kind, p.Name, n)
				}
			}
			if s, ok := p.Default.(string); ok && p.Type == dispatch.ParamEnum {
				found := false
				for _, e := range p.Enum {
					if e == s {
						found = true
					}
				}
				if !found {
					t.Fatalf("%s: param %q default %q is not one of its enum values", spec.Kind, p.Name, s)
				}
			}
		}
	}
}

func TestEverySpecIsAdvertisedForEveryKind(t *testing.T) {
	r := specRouter(t)
	if len(r.Specs()) != len(r.Kinds()) {
		t.Fatalf("specs = %d, kinds = %d", len(r.Specs()), len(r.Kinds()))
	}
}

// Pagination used to be spelled five different ways. The canonical name is
// "limit"; the old spellings have to keep working for existing callers.
func TestLegacyPaginationNamesStillReachHandlers(t *testing.T) {
	spec, ok := specRouter(t).SpecFor("app.list")
	if !ok {
		t.Fatal("app.list is not registered")
	}

	bound, _, ignored, err := spec.Bind(map[string]json.RawMessage{
		"max_results": json.RawMessage("42"),
	})
	if err != nil {
		t.Fatalf("bind: %v", err)
	}
	if got := string(bound["limit"]); got != "42" {
		t.Fatalf("limit = %s, want the aliased 42", got)
	}
	if len(ignored) != 0 {
		t.Fatalf("a legacy name must not be reported as ignored: %v", ignored)
	}
}

func TestRiskyKindsAreMarkedRiskyInTheirSpecs(t *testing.T) {
	wantRisky := []string{
		"ps.exec", "fs.upload", "fs.mkdir", "fs.delete", "proc.kill",
		"svc.control", "tunnel.open", "pico.bootsel", "pico.flash",
		"pico.save", "pico.reset",
	}
	r := specRouter(t)
	for _, kind := range wantRisky {
		spec, ok := r.SpecFor(kind)
		if !ok {
			t.Fatalf("%s is not registered", kind)
		}
		if len(spec.Params) == 0 && spec.Label == "" {
			continue
		}
		if !spec.Risky {
			t.Fatalf("%s must be advertised as risky", kind)
		}
	}
}

func TestEnumParamsRejectValuesOutsideTheirSet(t *testing.T) {
	spec, ok := specRouter(t).SpecFor("svc.control")
	if !ok {
		t.Fatal("svc.control is not registered")
	}
	_, _, _, err := spec.Bind(map[string]json.RawMessage{
		"name":   json.RawMessage(`"Spooler"`),
		"action": json.RawMessage(`"obliterate"`),
	})
	if err == nil {
		t.Fatal("expected an enum rejection")
	}
	for _, want := range []string{"start", "stop", "restart"} {
		if !strings.Contains(err.Error(), want) {
			t.Fatalf("error should name %q: %v", want, err)
		}
	}
}

func TestSpecBindingRunsBeforeTheHandler(t *testing.T) {
	r := specRouter(t)
	out := r.Dispatch(context.Background(), "proc.kill", map[string]json.RawMessage{
		"pid": json.RawMessage(`"not-a-number"`),
	})
	if out.OK {
		t.Fatal("a string pid must be rejected, not coerced to zero")
	}
	if !strings.Contains(out.Error, "must be a number") {
		t.Fatalf("error = %q", out.Error)
	}
}

// SPDX-License-Identifier: GPL-3.0-or-later
package dispatch

import (
	"context"
	"encoding/json"
	"reflect"
	"testing"
)

func TestRouterKindsReturnsSortedKinds(t *testing.T) {
	r := New()
	r.Register("sys.uptime", noopHandler)
	r.Register("fs.read", noopHandler)
	r.Register("net.ping", noopHandler)

	got := r.Kinds()
	want := []string{"fs.read", "net.ping", "sys.uptime"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("Kinds() = %#v, want %#v", got, want)
	}
}

func noopHandler(context.Context, map[string]json.RawMessage) (interface{}, error) {
	return nil, nil
}

func TestDispatchReturnsOutcomeForHandlerError(t *testing.T) {
	r := New()
	r.Register("boom", func(context.Context, map[string]json.RawMessage) (interface{}, error) {
		return nil, context.Canceled
	})
	out := r.Dispatch(context.Background(), "boom", nil)
	if out.OK || out.Error == "" {
		t.Fatalf("Dispatch(err) = %#v, want OK=false with Error", out)
	}
}

func TestDispatchUnknownKind(t *testing.T) {
	out := New().Dispatch(context.Background(), "nope", nil)
	if out.OK || out.Error == "" {
		t.Fatalf("Dispatch(unknown) = %#v, want OK=false with Error", out)
	}
}

// A panicking handler must not crash the host: Dispatch recovers it into a
// failed Outcome so the session loop keeps running.
func TestDispatchRecoversHandlerPanic(t *testing.T) {
	r := New()
	r.Register("panic", func(context.Context, map[string]json.RawMessage) (interface{}, error) {
		panic("nil deref in a capability")
	})
	out := r.Dispatch(context.Background(), "panic", nil)
	if out.OK {
		t.Fatal("panicking handler returned OK=true")
	}
	if out.Error == "" {
		t.Fatal("panicking handler produced no error text")
	}
}

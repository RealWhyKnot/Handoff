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

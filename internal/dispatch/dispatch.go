// SPDX-License-Identifier: GPL-3.0-or-later
//
// Package dispatch maps command kinds to handler functions. Handlers
// produce a result payload that the bridge sends back to the relay.

package dispatch

import (
	"context"
	"encoding/json"
	"fmt"
	"time"
)

// Handler is the contract every capability implements. The args map
// holds the kind-specific extras off the wire; the handler decodes
// what it needs. Result is anything JSON-marshalable.
type Handler func(ctx context.Context, args map[string]json.RawMessage) (result interface{}, err error)

// Router is the central registry.
type Router struct {
	handlers map[string]Handler
}

// New returns an empty router.
func New() *Router {
	return &Router{handlers: map[string]Handler{}}
}

// Register adds a handler for the given kind. Re-registering overwrites
// silently -- helpful for tests, harmless in production.
func (r *Router) Register(kind string, h Handler) {
	r.handlers[kind] = h
}

// Kinds returns the registered command kinds in no particular order.
func (r *Router) Kinds() []string {
	out := make([]string, 0, len(r.handlers))
	for k := range r.handlers {
		out = append(out, k)
	}
	return out
}

// Outcome is what the session loop sends back to the relay for each
// command. Elapsed wall-clock is measured around the handler call.
type Outcome struct {
	OK        bool
	Result    interface{}
	Error     string
	ElapsedMs int64
}

// Dispatch resolves a command kind to its handler and runs it.
// Unknown kinds produce an Outcome with OK=false; the loop continues.
func (r *Router) Dispatch(ctx context.Context, kind string, args map[string]json.RawMessage) Outcome {
	t0 := time.Now()
	h, ok := r.handlers[kind]
	if !ok {
		return Outcome{
			OK:        false,
			Error:     fmt.Sprintf("unknown kind: %s", kind),
			ElapsedMs: time.Since(t0).Milliseconds(),
		}
	}
	res, err := h(ctx, args)
	out := Outcome{
		OK:        err == nil,
		Result:    res,
		ElapsedMs: time.Since(t0).Milliseconds(),
	}
	if err != nil {
		out.Error = err.Error()
	}
	return out
}

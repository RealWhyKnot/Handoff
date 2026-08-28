// SPDX-License-Identifier: GPL-3.0-or-later
//
// Package dispatch maps command kinds to handler functions. Handlers
// produce a result payload that the bridge sends back to the relay.

package dispatch

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"sort"
	"time"
)

// Handler is the contract every capability implements. The args map
// holds the kind-specific extras off the wire; the handler decodes
// what it needs. Result is anything JSON-marshalable.
type Handler func(ctx context.Context, args map[string]json.RawMessage) (result interface{}, err error)

// Router is the central registry.
type Router struct {
	handlers map[string]Handler
	specs    map[string]Spec
}

// New returns an empty router.
func New() *Router {
	return &Router{handlers: map[string]Handler{}, specs: map[string]Spec{}}
}

// Register adds a handler for the given kind with no parameter contract.
// Arguments reach the handler exactly as they arrived.
func (r *Router) Register(kind string, h Handler) {
	r.handlers[kind] = h
	r.specs[kind] = Spec{Kind: kind}
}

// RegisterSpec adds a handler whose arguments are validated and normalized
// against spec before the handler runs, and whose schema is advertised to the
// relay so operators and agents see the same contract the host enforces.
func (r *Router) RegisterSpec(spec Spec, h Handler) {
	r.handlers[spec.Kind] = h
	r.specs[spec.Kind] = spec
}

// Kinds returns the registered command kinds in stable lexical order.
func (r *Router) Kinds() []string {
	out := make([]string, 0, len(r.handlers))
	for k := range r.handlers {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

// Specs returns every registered spec in stable lexical order by kind.
func (r *Router) Specs() []Spec {
	out := make([]Spec, 0, len(r.specs))
	for _, k := range r.Kinds() {
		if s, ok := r.specs[k]; ok {
			out = append(out, s)
		}
	}
	return out
}

// SpecFor returns the spec registered for a kind.
func (r *Router) SpecFor(kind string) (Spec, bool) {
	s, ok := r.specs[kind]
	return s, ok
}

// Outcome is what the session loop sends back to the relay for each
// command. Elapsed wall-clock is measured around the handler call.
type Outcome struct {
	OK        bool
	Result    interface{}
	Error     string
	ElapsedMs int64
	Detail    interface{}
	Clamped   []Clamp
	Ignored   []string
}

// Dispatch resolves a command kind to its handler and runs it.
// Unknown kinds produce an Outcome with OK=false; the loop continues.
// A panic in any handler is recovered into a failed Outcome so one bad
// command can never take down the host session.
func (r *Router) Dispatch(ctx context.Context, kind string, args map[string]json.RawMessage) (out Outcome) {
	t0 := time.Now()
	h, ok := r.handlers[kind]
	if !ok {
		return Outcome{
			OK:        false,
			Error:     fmt.Sprintf("unknown kind: %s", kind),
			ElapsedMs: time.Since(t0).Milliseconds(),
		}
	}
	defer func() {
		if p := recover(); p != nil {
			out = Outcome{
				OK:        false,
				Error:     fmt.Sprintf("internal error handling %s: %v", kind, p),
				ElapsedMs: time.Since(t0).Milliseconds(),
			}
		}
	}()

	var clamped []Clamp
	var ignored []string
	if spec, ok := r.specs[kind]; ok && len(spec.Params) > 0 {
		bound, c, ig, err := spec.Bind(args)
		if err != nil {
			return Outcome{
				OK:        false,
				Error:     err.Error(),
				ElapsedMs: time.Since(t0).Milliseconds(),
			}
		}
		args, clamped, ignored = bound, c, ig
	}

	res, err := h(ctx, args)
	out = Outcome{
		OK:        err == nil,
		Result:    res,
		ElapsedMs: time.Since(t0).Milliseconds(),
		Clamped:   clamped,
		Ignored:   ignored,
	}
	if err != nil {
		out.Error = err.Error()
		var fail *Failure
		if errors.As(err, &fail) {
			out.Error = fail.Message
			out.Detail = fail.Detail()
		}
	}
	return out
}

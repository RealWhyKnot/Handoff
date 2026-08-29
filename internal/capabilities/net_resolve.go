// SPDX-License-Identifier: GPL-3.0-or-later
//
// net.resolve answers "is it DNS?". net.dns-cache only shows what Windows has
// already cached, so there was no way to ask what a name resolves to now.

package capabilities

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
	"strings"
	"time"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

func RegisterNetResolve(r *dispatch.Router) {
	r.RegisterSpec(dispatch.Spec{
		Kind:        "net.resolve",
		Label:       "Resolve name",
		Description: "Look up a hostname's current DNS answer.",
		Params: []dispatch.Param{
			{Name: "name", Type: dispatch.ParamString, Required: true, Description: "Hostname to resolve."},
			{Name: "type", Type: dispatch.ParamEnum, Enum: []string{"a", "aaaa", "any"}, Default: "any"},
		},
	}, netResolve)
}

func netResolve(ctx context.Context, args map[string]json.RawMessage) (interface{}, error) {
	var name, want string
	if v, ok := args["name"]; ok {
		_ = json.Unmarshal(v, &name)
	}
	if v, ok := args["type"]; ok {
		_ = json.Unmarshal(v, &want)
	}
	name = strings.TrimSpace(name)
	if name == "" {
		return nil, fmt.Errorf("net.resolve: 'name' is required")
	}
	if len(name) > 253 {
		return nil, fmt.Errorf("net.resolve: 'name' is too long")
	}
	if !validTarget(name) {
		return nil, fmt.Errorf("net.resolve: 'name' contains characters that are not valid in a hostname")
	}
	if want == "" {
		want = "any"
	}

	lookupCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()

	started := time.Now()
	addrs, err := net.DefaultResolver.LookupIPAddr(lookupCtx, name)
	elapsed := time.Since(started).Milliseconds()
	if err != nil {
		return map[string]interface{}{
			"name":       name,
			"type":       want,
			"resolved":   false,
			"addresses":  []string{},
			"error":      err.Error(),
			"elapsed_ms": elapsed,
		}, nil
	}

	out := make([]string, 0, len(addrs))
	for _, a := range addrs {
		isV4 := a.IP.To4() != nil
		switch want {
		case "a":
			if !isV4 {
				continue
			}
		case "aaaa":
			if isV4 {
				continue
			}
		}
		out = append(out, a.IP.String())
	}

	cname, _ := net.DefaultResolver.LookupCNAME(lookupCtx, name)
	return map[string]interface{}{
		"name":       name,
		"type":       want,
		"resolved":   len(out) > 0,
		"addresses":  out,
		"cname":      strings.TrimSuffix(cname, "."),
		"elapsed_ms": elapsed,
	}, nil
}

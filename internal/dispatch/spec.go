// SPDX-License-Identifier: GPL-3.0-or-later

package dispatch

import (
	"encoding/json"
	"fmt"
	"strings"
)

type ParamType string

const (
	ParamString ParamType = "string"
	ParamInt    ParamType = "int"
	ParamBool   ParamType = "bool"
	ParamEnum   ParamType = "enum"
	ParamBytes  ParamType = "bytes_base64"
)

// Param describes one argument a command accepts. It is both the validation
// contract the binder enforces and the schema the host advertises, so a
// capability cannot document one shape and accept another.
type Param struct {
	Name        string      `json:"name"`
	Type        ParamType   `json:"type"`
	Required    bool        `json:"required,omitempty"`
	Default     interface{} `json:"default,omitempty"`
	Min         *int        `json:"min,omitempty"`
	Max         *int        `json:"max,omitempty"`
	Enum        []string    `json:"enum,omitempty"`
	Aliases     []string    `json:"aliases,omitempty"`
	MaxBytes    int         `json:"max_bytes,omitempty"`
	Description string      `json:"description,omitempty"`
}

// Spec describes one command kind.
type Spec struct {
	Kind             string  `json:"kind"`
	Label            string  `json:"label,omitempty"`
	Description      string  `json:"description,omitempty"`
	Risky            bool    `json:"risky,omitempty"`
	DefaultTimeoutMS int     `json:"default_timeout_ms,omitempty"`
	Params           []Param `json:"params,omitempty"`
}

// Clamp records a value the binder pulled into range, so a caller can tell the
// difference between the result it asked for and the result it got.
type Clamp struct {
	Param string `json:"param"`
	From  int    `json:"from"`
	To    int    `json:"to"`
}

func IntPtr(v int) *int { return &v }

// Bind validates args against the spec and returns a normalized copy: aliases
// resolved to canonical names, defaults filled in, ints clamped to range.
// Unknown keys are reported rather than silently dropped.
func (s Spec) Bind(args map[string]json.RawMessage) (map[string]json.RawMessage, []Clamp, []string, error) {
	out := make(map[string]json.RawMessage, len(args)+len(s.Params))
	for k, v := range args {
		out[k] = v
	}

	var clamped []Clamp
	known := map[string]bool{}

	for _, p := range s.Params {
		known[p.Name] = true
		raw, present := out[p.Name]
		if !present {
			for _, alias := range p.Aliases {
				known[alias] = true
				if v, ok := out[alias]; ok {
					raw, present = v, true
					out[p.Name] = v
					delete(out, alias)
					break
				}
			}
		}

		if present && isJSONNull(raw) {
			delete(out, p.Name)
			present = false
		}

		if !present {
			if p.Required {
				return nil, nil, nil, fmt.Errorf("%s: '%s' is required", s.Kind, p.Name)
			}
			if p.Default != nil {
				enc, err := json.Marshal(p.Default)
				if err != nil {
					return nil, nil, nil, fmt.Errorf("%s: bad default for '%s': %w", s.Kind, p.Name, err)
				}
				out[p.Name] = enc
			}
			continue
		}

		normalized, clamp, err := bindOne(s.Kind, p, raw)
		if err != nil {
			return nil, nil, nil, err
		}
		out[p.Name] = normalized
		if clamp != nil {
			clamped = append(clamped, *clamp)
		}
	}

	var ignored []string
	if len(s.Params) > 0 {
		for k := range out {
			if !known[k] {
				ignored = append(ignored, k)
			}
		}
	}
	return out, clamped, ignored, nil
}

func bindOne(kind string, p Param, raw json.RawMessage) (json.RawMessage, *Clamp, error) {
	switch p.Type {
	case ParamInt:
		var n int
		if err := json.Unmarshal(raw, &n); err != nil {
			return nil, nil, fmt.Errorf("%s: '%s' must be a number", kind, p.Name)
		}
		original := n
		if p.Min != nil && n < *p.Min {
			n = *p.Min
		}
		if p.Max != nil && n > *p.Max {
			n = *p.Max
		}
		enc, err := json.Marshal(n)
		if err != nil {
			return nil, nil, err
		}
		if n != original {
			return enc, &Clamp{Param: p.Name, From: original, To: n}, nil
		}
		return enc, nil, nil

	case ParamBool:
		var b bool
		if err := json.Unmarshal(raw, &b); err != nil {
			return nil, nil, fmt.Errorf("%s: '%s' must be true or false", kind, p.Name)
		}
		return raw, nil, nil

	case ParamEnum:
		var v string
		if err := json.Unmarshal(raw, &v); err != nil {
			return nil, nil, fmt.Errorf("%s: '%s' must be a string", kind, p.Name)
		}
		for _, allowed := range p.Enum {
			if v == allowed {
				return raw, nil, nil
			}
		}
		return nil, nil, fmt.Errorf("%s: '%s' must be one of: %s", kind, p.Name, strings.Join(p.Enum, ", "))

	case ParamString, ParamBytes:
		var v string
		if err := json.Unmarshal(raw, &v); err != nil {
			return nil, nil, fmt.Errorf("%s: '%s' must be a string", kind, p.Name)
		}
		if p.MaxBytes > 0 && len(v) > p.MaxBytes {
			return nil, nil, fmt.Errorf("%s: '%s' is %d bytes; cap is %d", kind, p.Name, len(v), p.MaxBytes)
		}
		return raw, nil, nil
	}
	return raw, nil, nil
}

func isJSONNull(raw json.RawMessage) bool {
	return len(raw) == 4 && string(raw) == "null"
}

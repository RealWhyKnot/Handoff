// SPDX-License-Identifier: GPL-3.0-or-later
//
// Shared spec fragments. Row limits were spelled five different ways across
// the capability set (max_results, max_events, max_entries, top, lines);
// "limit" is now the canonical name everywhere it means "how many rows", and
// the older spellings stay accepted as aliases.

package capabilities

import "github.com/RealWhyKnot/Handoff/internal/dispatch"

func limitParam(def, min, max int, aliases ...string) dispatch.Param {
	if len(aliases) == 0 {
		aliases = []string{"max_results"}
	}
	return dispatch.Param{
		Name:        "limit",
		Type:        dispatch.ParamInt,
		Default:     def,
		Min:         dispatch.IntPtr(min),
		Max:         dispatch.IntPtr(max),
		Aliases:     aliases,
		Description: "Maximum rows to return.",
	}
}

func plainSpec(kind, label, description string) dispatch.Spec {
	return dispatch.Spec{Kind: kind, Label: label, Description: description}
}

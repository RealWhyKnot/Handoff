// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterAll wires every shipped capability into the router. Add new
// register functions here when introducing a new category. The bridge is
// captured so tunnel handlers can push bytes back without going through the
// dispatcher's command_result return path.
func RegisterAll(r *dispatch.Router, bridge TunnelBridge) {
	resetRiskConsent()
	RegisterSys(r)
	RegisterHw(r)
	RegisterStorage(r)
	RegisterNet(r)
	RegisterNetProbes(r)
	RegisterNetCurl(r)
	RegisterProc(r)
	RegisterEvt(r)
	RegisterDrv(r)
	RegisterFs(r)
	RegisterFsWrite(r)
	RegisterFsText(r)
	RegisterNetResolve(r)
	RegisterScreenshot(r)
	RegisterReg(r)
	RegisterTask(r)
	RegisterSec(r)
	RegisterApp(r)
	RegisterStartup(r)
	RegisterPico(r)
	RegisterPs(r)
	RegisterTunnel(r, bridge)
	RegisterControl(r)
}

// RegisterControl advertises the cancellation kind. It is handled on the
// receive path ahead of the dispatcher, so without this it never appeared in
// the advertised command list and an agent had no way to discover that a
// running command can be cancelled at all.
func RegisterControl(r *dispatch.Router) {
	r.RegisterSpec(dispatch.Spec{
		Kind:        "control.cancel",
		Label:       "Cancel command",
		Description: "Ask the host to stop a running command.",
		Params: []dispatch.Param{
			{Name: "target_id", Type: dispatch.ParamString, Required: true, Description: "Id of the command to stop."},
		},
	}, controlCancelPlaceholder)
}

// controlCancelPlaceholder exists only so control.cancel is advertised. Real
// cancellation is handled before dispatch, because a cancel that queued behind
// the command it is cancelling would never arrive.
func controlCancelPlaceholder(context.Context, map[string]json.RawMessage) (interface{}, error) {
	return nil, fmt.Errorf("control.cancel is handled by the session loop")
}

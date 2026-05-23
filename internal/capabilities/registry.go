// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import "github.com/RealWhyKnot/Handoff/internal/dispatch"

// RegisterAll wires every shipped capability into the router. Add new
// register functions here when introducing a new category. The bridge is
// captured so tunnel handlers can push bytes back without going through the
// dispatcher's command_result return path.
func RegisterAll(r *dispatch.Router, bridge TunnelBridge) {
	resetRiskConsent()
	RegisterSys(r)
	RegisterHw(r)
	RegisterNet(r)
	RegisterNetProbes(r)
	RegisterNetCurl(r)
	RegisterProc(r)
	RegisterEvt(r)
	RegisterDrv(r)
	RegisterFs(r)
	RegisterFsWrite(r)
	RegisterReg(r)
	RegisterTask(r)
	RegisterSec(r)
	RegisterApp(r)
	RegisterPico(r)
	RegisterPs(r)
	RegisterTunnel(r, bridge)
}

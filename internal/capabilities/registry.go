// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import "github.com/RealWhyKnot/Handoff/internal/dispatch"

// RegisterAll wires every shipped capability into the router. Add new
// register functions here when introducing a new category.
func RegisterAll(r *dispatch.Router) {
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
}

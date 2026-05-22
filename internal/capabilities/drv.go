// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterDrv wires the drv.list driver-inventory handler.
func RegisterDrv(r *dispatch.Router) {
	r.Register("drv.list", drvList)
}

func drvList(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	// Win32_PnpSignedDriver returns the currently-bound driver per device,
	// which is the working set vs the staged-but-inactive store that
	// pnputil enumerates. Inventory snapshots want the bound view.
	return runPwshJSON(ctx, `Get-CimInstance Win32_PnpSignedDriver | Select-Object DeviceName,DeviceClass,DriverVersion,DriverDate,Manufacturer,Signer,InfName | ConvertTo-Json -Compress`)
}

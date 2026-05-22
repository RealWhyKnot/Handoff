// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterHw wires the hw.* read-only hardware-inventory handlers.
func RegisterHw(r *dispatch.Router) {
	r.Register("hw.cpu", hwCpu)
	r.Register("hw.ram", hwRam)
	r.Register("hw.usb", hwUsb)
	r.Register("hw.disks", hwDisks)
	r.Register("hw.gpu", hwGpu)
}

func hwCpu(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	return runPwshJSON(ctx, `Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed,Manufacturer | ConvertTo-Json -Compress`)
}

func hwRam(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	return runPwshJSON(ctx, `Get-CimInstance Win32_PhysicalMemory | Select-Object DeviceLocator,@{n='SizeGB';e={[math]::Round($_.Capacity/1GB,2)}},Speed,Manufacturer,PartNumber | ConvertTo-Json -Compress`)
}

func hwUsb(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	return runPwshJSON(ctx, `Get-PnpDevice -PresentOnly | Where-Object {$_.InstanceId -like 'USB\*'} | Select-Object InstanceId,FriendlyName,Status,Class,Manufacturer | ConvertTo-Json -Compress`)
}

func hwDisks(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
$disks = Get-PhysicalDisk | Select-Object DeviceId,FriendlyName,SerialNumber,@{n='SizeGB';e={[math]::Round($_.Size/1GB,2)}},MediaType,HealthStatus,OperationalStatus,BusType
$disks | ConvertTo-Json -Compress
`
	return runPwshJSON(ctx, script)
}

func hwGpu(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	return runPwshJSON(ctx, `Get-CimInstance Win32_VideoController | Select-Object Name,AdapterRAM,DriverVersion,VideoProcessor,Status | ConvertTo-Json -Compress`)
}

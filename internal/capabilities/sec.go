// SPDX-License-Identifier: GPL-3.0-or-later
package capabilities

import (
	"context"
	"encoding/json"

	"github.com/RealWhyKnot/Handoff/internal/dispatch"
)

// RegisterSec wires read-only security / patch state inspectors.
//
// update.history reports the last ~50 Windows update entries (via the
// Microsoft.Update.Session COM object). defender.status reports current
// Windows Defender configuration and signature freshness.
func RegisterSec(r *dispatch.Router) {
	r.Register("update.history", updateHistory)
	r.Register("defender.status", defenderStatus)
}

func updateHistory(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
$entries = @()
try {
    $session = New-Object -ComObject Microsoft.Update.Session
    $searcher = $session.CreateUpdateSearcher()
    $count = [int]$searcher.GetTotalHistoryCount()
    $want = [Math]::Min($count, 50)
    if ($want -gt 0) {
        $hist = $searcher.QueryHistory(0, $want)
        for ($i = 0; $i -lt $hist.Count; $i++) {
            $h = $hist.Item($i)
            $entries += [ordered]@{
                title       = [string]$h.Title
                description = [string]$h.Description
                operation   = [int]$h.Operation
                result_code = [int]$h.ResultCode
                hresult     = [string]$h.HResult
                date_utc    = if ($h.Date) { ([datetime]$h.Date).ToUniversalTime().ToString("o") } else { $null }
                client_app  = [string]$h.ClientApplicationID
                support_url = [string]$h.SupportUrl
            }
        }
    }
} catch {
    [ordered]@{
        error = $_.Exception.Message
        entries = @()
    } | ConvertTo-Json -Compress -Depth 5
    return
}

[ordered]@{
    count   = $entries.Count
    entries = @($entries)
} | ConvertTo-Json -Compress -Depth 5
`
	return runPwshJSON(ctx, script)
}

func defenderStatus(ctx context.Context, _ map[string]json.RawMessage) (interface{}, error) {
	script := `
try {
    $s = Get-MpComputerStatus -ErrorAction Stop
    $pref = Get-MpPreference -ErrorAction SilentlyContinue
    [ordered]@{
        antivirus_enabled                 = [bool]$s.AntivirusEnabled
        antispyware_enabled               = [bool]$s.AntispywareEnabled
        realtime_protection_enabled       = [bool]$s.RealTimeProtectionEnabled
        ioav_protection_enabled           = [bool]$s.IoavProtectionEnabled
        on_access_protection_enabled      = [bool]$s.OnAccessProtectionEnabled
        behavior_monitor_enabled          = [bool]$s.BehaviorMonitorEnabled
        nis_enabled                       = [bool]$s.NISEnabled
        tamper_protection_enabled         = [bool]$s.IsTamperProtected
        antivirus_signature_version       = [string]$s.AntivirusSignatureVersion
        antivirus_signature_age_days      = [int]$s.AntivirusSignatureAge
        last_quickscan_utc                = if ($s.QuickScanEndTime) { $s.QuickScanEndTime.ToUniversalTime().ToString("o") } else { $null }
        last_fullscan_utc                 = if ($s.FullScanEndTime) { $s.FullScanEndTime.ToUniversalTime().ToString("o") } else { $null }
        computer_state                    = [int]$s.ComputerState
        defender_engine_version           = [string]$s.AMEngineVersion
        defender_product_version          = [string]$s.AMProductVersion
        full_scan_required                = if ($null -ne $s.FullScanRequired) { [bool]$s.FullScanRequired } else { $null }
        cloud_block_level                 = if ($pref) { [string]$pref.CloudBlockLevel } else { $null }
        scan_avg_cpu_load_factor          = if ($pref) { [int]$pref.ScanAvgCPULoadFactor } else { $null }
        exclusion_paths_count             = if ($pref -and $pref.ExclusionPath) { @($pref.ExclusionPath).Count } else { 0 }
    } | ConvertTo-Json -Compress -Depth 4
} catch {
    [ordered]@{
        available = $false
        error = $_.Exception.Message
    } | ConvertTo-Json -Compress
}
`
	return runPwshJSON(ctx, script)
}

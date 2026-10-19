use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::pwsh::run_pwsh_json;

const UPDATE_HISTORY: &str = r##"
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
"##;

const DEFENDER_STATUS: &str = r##"
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
"##;

const LOCAL_ADMINS: &str = r##"
if (-not (Get-Command Get-LocalGroupMember -ErrorAction SilentlyContinue)) {
    [ordered]@{
        available = $false
        group = 'Administrators'
        error = 'Get-LocalGroupMember is not available in this PowerShell host'
        members = @()
    } | ConvertTo-Json -Compress
    return
}

try {
    $members = Get-LocalGroupMember -Group 'Administrators' -ErrorAction Stop |
        Sort-Object ObjectClass, Name |
        ForEach-Object {
            [ordered]@{
                name = [string]$_.Name
                object_class = [string]$_.ObjectClass
                sid = if ($_.SID) { [string]$_.SID.Value } else { $null }
                principal_source = if ($null -ne $_.PrincipalSource) { [string]$_.PrincipalSource } else { $null }
            }
        }
    [ordered]@{
        available = $true
        group = 'Administrators'
        count = @($members).Count
        members = @($members)
    } | ConvertTo-Json -Compress -Depth 4
} catch {
    [ordered]@{
        available = $false
        group = 'Administrators'
        error = $_.Exception.Message
        members = @()
    } | ConvertTo-Json -Compress
}
"##;

pub fn register(r: &mut Router) {
    r.register("update.history", handler(update_history));
    r.register("defender.status", handler(defender_status));
    r.register("sec.local-admins", handler(sec_local_admins));
}

async fn run(ctx: Ctx, script: &str) -> CapResult {
    run_pwsh_json(&ctx.cancel, script)
        .await
        .map_err(HandlerError::from)
}

async fn update_history(ctx: Ctx, _args: Args) -> CapResult {
    run(ctx, UPDATE_HISTORY).await
}

async fn defender_status(ctx: Ctx, _args: Args) -> CapResult {
    run(ctx, DEFENDER_STATUS).await
}

async fn sec_local_admins(ctx: Ctx, _args: Args) -> CapResult {
    run(ctx, LOCAL_ADMINS).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_the_security_kinds_as_plain_specs() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(
            r.kinds(),
            vec!["defender.status", "sec.local-admins", "update.history"]
        );
        for spec in r.specs() {
            assert!(spec.params.is_empty() && spec.label.is_empty());
        }
    }

    #[test]
    fn scripts_end_in_json_output() {
        for script in [UPDATE_HISTORY, DEFENDER_STATUS, LOCAL_ADMINS] {
            assert!(script.contains("ConvertTo-Json -Compress"));
        }
    }
}

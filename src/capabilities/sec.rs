use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::pwsh_out::run_pwsh_json;

const UPDATE_HISTORY: &str = r##"
$operations = @{ 1 = 'Installation'; 2 = 'Uninstallation'; 3 = 'Other' }
$results = @{ 0 = 'NotStarted'; 1 = 'InProgress'; 2 = 'Succeeded'; 3 = 'SucceededWithErrors'; 4 = 'Failed'; 5 = 'Aborted' }
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
                operation   = if ($operations.ContainsKey([int]$h.Operation)) { $operations[[int]$h.Operation] } else { [string]$h.Operation }
                result_code = if ($results.ContainsKey([int]$h.ResultCode)) { $results[[int]$h.ResultCode] } else { [string]$h.ResultCode }
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
$stateFlags = [ordered]@{ 1 = 'PendingFullScan'; 2 = 'PendingReboot'; 4 = 'PendingManualSteps'; 8 = 'PendingOfflineScan'; 16 = 'PendingCriticalFailure' }
$blockLevels = @{ 0 = 'Default'; 1 = 'Moderate'; 2 = 'High'; 4 = 'HighPlus'; 6 = 'ZeroTolerance' }
function State-Name($v) {
    $n = [int]$v
    if ($n -eq 0) { return 'Clean' }
    $parts = @($stateFlags.Keys | Where-Object { $n -band $_ } | ForEach-Object { $stateFlags[$_] })
    if ($parts.Count) { $parts -join ', ' } else { [string]$n }
}
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
        computer_state                    = State-Name $s.ComputerState
        defender_engine_version           = [string]$s.AMEngineVersion
        defender_product_version          = [string]$s.AMProductVersion
        full_scan_required                = if ($null -ne $s.FullScanRequired) { [bool]$s.FullScanRequired } else { $null }
        cloud_block_level                 = if ($pref) { $b = [int]$pref.CloudBlockLevel; if ($blockLevels.ContainsKey($b)) { $blockLevels[$b] } else { [string]$b } } else { $null }
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
    super::register_specs(
        r,
        include_str!("specs/sec.json"),
        vec![
            ("update.history", handler(update_history)),
            ("defender.status", handler(defender_status)),
            ("sec.local-admins", handler(sec_local_admins)),
        ],
    );
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
    fn registers_the_security_kinds_with_labelled_specs() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(
            r.kinds(),
            vec!["defender.status", "sec.local-admins", "update.history"]
        );
        for spec in r.specs() {
            assert!(!spec.label.is_empty(), "{} has no label", spec.kind);
        }
    }

    #[test]
    fn scripts_end_in_json_output() {
        for script in [UPDATE_HISTORY, DEFENDER_STATUS, LOCAL_ADMINS] {
            assert!(script.contains("ConvertTo-Json -Compress"));
        }
    }

    #[test]
    fn enum_values_are_reported_by_name() {
        assert!(UPDATE_HISTORY.contains("2 = 'Succeeded'"));
        assert!(UPDATE_HISTORY.contains("1 = 'Installation'"));
        assert!(DEFENDER_STATUS.contains("State-Name $s.ComputerState"));
        assert!(DEFENDER_STATUS.contains("6 = 'ZeroTolerance'"));
        assert!(!DEFENDER_STATUS.contains("[int]$s.ComputerState"));
    }
}

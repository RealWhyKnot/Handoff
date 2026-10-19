use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::proc::{fill, go_quote, int_arg, string_arg};
use super::pwsh::ps_single_quote;
use super::pwsh_out::run_pwsh_json;

const SNAPSHOT_TEMPLATE: &str = r##"
$start = (Get-Date).AddMinutes(-@@SINCE@@)
$channel = @@CHANNEL@@
$provider = @@PROVIDER@@
$ids = @(@@IDS@@)
if (-not (Get-WinEvent -ListLog $channel -ErrorAction SilentlyContinue)) {
    [Console]::Error.WriteLine("There is no event log named '$channel' on this PC.")
    exit 2
}
$filter = @{ LogName = $channel; StartTime = $start }
$levels = @(@@LEVELS@@)
if ($levels.Count) { $filter.Level = $levels }
if ($provider) { $filter.ProviderName = $provider }
if ($ids.Count) { $filter.Id = $ids }
try {
    $raw = @(Get-WinEvent -FilterHashtable $filter -MaxEvents @@MAX@@ -ErrorAction Stop)
} catch {
    if ($_.FullyQualifiedErrorId -notlike 'NoMatchingEventsFound*') {
        [Console]::Error.WriteLine($_.Exception.Message)
        exit 3
    }
    $raw = @()
}
$events = @($raw | Select-Object @{n='time';e={$_.TimeCreated.ToUniversalTime().ToString("o")}},LevelDisplayName,Id,ProviderName,Message)
ConvertTo-Json -InputObject $events -Compress -Depth 4
"##;

const CRASHES_TEMPLATE: &str = r##"
$since = (Get-Date).AddDays(-@@DAYS@@)
$max = @@MAX@@
function First-Line($m) {
    if (-not $m) { return $null }
    $line = ([string]$m -split "`r?`n")[0].Trim()
    if ($line.Length -gt 300) { $line.Substring(0, 300) } else { $line }
}
function Prop($e, $i) {
    if ($e.Properties.Count -gt $i) { [string]$e.Properties[$i].Value } else { $null }
}
function Find($log, $provider, $id) {
    try {
        @(Get-WinEvent -FilterHashtable @{ LogName = $log; ProviderName = $provider; Id = $id; StartTime = $since } -MaxEvents $max -ErrorAction Stop)
    } catch { @() }
}
function When($e) { $e.TimeCreated.ToUniversalTime().ToString("o") }
$rows = @()
foreach ($e in Find 'System' 'Microsoft-Windows-Kernel-Power' 41) {
    $code = [uint64]0
    [void][uint64]::TryParse((Prop $e 0), [ref]$code)
    $rows += [pscustomobject][ordered]@{
        time = When $e; kind = 'unexpected_restart'; log = 'System'; id = 41
        source = $e.ProviderName; summary = First-Line $e.Message
        bugcheck_code = if ($code) { '0x{0:X8}' -f $code } else { $null }
    }
}
foreach ($e in Find 'System' 'EventLog' 6008) {
    $rows += [pscustomobject][ordered]@{
        time = When $e; kind = 'unexpected_shutdown'; log = 'System'; id = 6008
        source = $e.ProviderName; summary = First-Line $e.Message
    }
}
foreach ($e in Find 'System' 'Microsoft-Windows-WER-SystemErrorReporting' 1001) {
    $rows += [pscustomobject][ordered]@{
        time = When $e; kind = 'bugcheck'; log = 'System'; id = 1001
        source = $e.ProviderName; summary = First-Line $e.Message
    }
}
foreach ($e in Find 'Application' 'Application Error' 1000) {
    $rows += [pscustomobject][ordered]@{
        time = When $e; kind = 'app_crash'; log = 'Application'; id = 1000
        source = $e.ProviderName; summary = First-Line $e.Message
        app = Prop $e 0; app_version = Prop $e 1; module = Prop $e 3; module_version = Prop $e 4
        exception_code = Prop $e 6; app_path = Prop $e 10
    }
}
foreach ($e in Find 'Application' 'Windows Error Reporting' 1001) {
    $rows += [pscustomobject][ordered]@{
        time = When $e; kind = 'error_report'; log = 'Application'; id = 1001
        source = $e.ProviderName; event_name = Prop $e 2
        signature = @(5..8 | ForEach-Object { Prop $e $_ } | Where-Object { $_ })
    }
}
$dumpDir = Join-Path $env:SystemRoot 'Minidump'
$dumps = [ordered]@{ path = $dumpDir; readable = $false; error = $null; files = @() }
if (Test-Path -LiteralPath $dumpDir) {
    try {
        $dumps.files = @(Get-ChildItem -LiteralPath $dumpDir -Filter *.dmp -File -ErrorAction Stop |
            Sort-Object LastWriteTime -Descending | Select-Object -First 50 | ForEach-Object {
                [ordered]@{ name = $_.Name; path = $_.FullName; size_bytes = [int64]$_.Length; modified_utc = $_.LastWriteTimeUtc.ToString("o") }
            })
        $dumps.readable = $true
    } catch [System.UnauthorizedAccessException] {
        $dumps.error = 'Only an administrator can list this folder.'
    } catch {
        if ($_.CategoryInfo.Category -eq 'PermissionDenied') {
            $dumps.error = 'Only an administrator can list this folder.'
        } else {
            $dumps.error = $_.Exception.Message
        }
    }
} else {
    $dumps.readable = $true
}
$full = Join-Path $env:SystemRoot 'MEMORY.DMP'
$fullDump = $null
try {
    $item = Get-Item -LiteralPath $full -ErrorAction Stop
    $fullDump = [ordered]@{ path = $full; size_bytes = [int64]$item.Length; modified_utc = $item.LastWriteTimeUtc.ToString("o") }
} catch {}
$all = @($rows)
[ordered]@{
    since_days = @@DAYS@@
    unexpected_shutdowns = @($all | Where-Object { $_.kind -eq 'unexpected_restart' -or $_.kind -eq 'unexpected_shutdown' }).Count
    blue_screens = @($all | Where-Object { $_.kind -eq 'bugcheck' }).Count
    app_crashes = @($all | Where-Object { $_.kind -eq 'app_crash' }).Count
    events = @($all | Sort-Object { $_.time } -Descending | Select-Object -First $max)
    minidumps = $dumps
    full_memory_dump = $fullDump
} | ConvertTo-Json -Compress -Depth 5
"##;

const PROVIDERS_TEMPLATE: &str = r##"
$prefix = @@PREFIX@@
$max = @@MAX@@
$logs = Get-WinEvent -ListLog * -ErrorAction SilentlyContinue
if ($prefix) {
    $needle = $prefix.ToLowerInvariant()
    $logs = $logs | Where-Object { $_.LogName.ToLowerInvariant().StartsWith($needle) }
}
$logs |
    Sort-Object LogName |
    Select-Object -First $max |
    Select-Object @{n='name';e={$_.LogName}},
                  @{n='enabled';e={[bool]$_.IsEnabled}},
                  @{n='mode';e={[string]$_.LogMode}},
                  @{n='record_count';e={[int64]$_.RecordCount}},
                  @{n='file_size';e={[int64]$_.FileSize}},
                  @{n='owning_provider';e={[string]$_.OwningProviderName}} |
    ConvertTo-Json -Compress -Depth 4
"##;

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/evt.json"),
        vec![
            ("evt.snapshot", handler(evt_snapshot)),
            ("evt.providers", handler(evt_providers)),
            ("evt.crashes", handler(evt_crashes)),
        ],
    );
}

pub fn valid_log_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " -_./()".contains(c))
}

pub fn event_ids(raw: &str) -> Result<Vec<u32>, HandlerError> {
    let mut ids = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.parse::<u32>() {
            Ok(id) if id <= 65535 => ids.push(id),
            _ => {
                return Err(HandlerError::plain(format!(
                    "evt.snapshot: event ID {} is not a number from 0 to 65535",
                    go_quote(part)
                )))
            }
        }
    }
    ids.sort_unstable();
    ids.dedup();
    if ids.len() > 20 {
        return Err(HandlerError::plain(
            "evt.snapshot: give at most 20 event IDs",
        ));
    }
    Ok(ids)
}

pub fn snapshot_script(args: &Args) -> Result<String, HandlerError> {
    let channel = string_arg(args, "channel", "System").trim().to_string();
    let channel = if channel.is_empty() {
        "System".to_string()
    } else {
        channel
    };
    let mut max = int_arg(args, "max_events", 200);
    if max <= 0 {
        max = 200;
    }
    let max = max.min(5000);
    let mut since = int_arg(args, "since_minutes", 60);
    if since <= 0 {
        since = 60;
    }
    let since = since.min(43200);
    let levels = match string_arg(args, "level", "all").to_lowercase().as_str() {
        "all" | "" => "",
        "critical" => "1",
        "error" => "1,2",
        "warning" => "1,2,3",
        "information" => "0,1,2,3,4",
        "verbose" => "0,1,2,3,4,5",
        _ => return Err(HandlerError::plain(
            "evt.snapshot: level must be all, critical, error, warning, information, or verbose",
        )),
    };
    if !valid_log_name(&channel) {
        return Err(HandlerError::plain(format!(
            "evt.snapshot: {} is not a valid event log name",
            go_quote(&channel)
        )));
    }
    let provider = string_arg(args, "provider", "").trim().to_string();
    if !provider.is_empty() && !valid_log_name(&provider) {
        return Err(HandlerError::plain(format!(
            "evt.snapshot: {} is not a valid event source name",
            go_quote(&provider)
        )));
    }
    let ids = event_ids(&string_arg(args, "event_id", ""))?
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    Ok(fill(
        SNAPSHOT_TEMPLATE,
        &[
            ("@@SINCE@@", &since.to_string()),
            ("@@CHANNEL@@", &ps_single_quote(&channel)),
            ("@@PROVIDER@@", &ps_single_quote(&provider)),
            ("@@IDS@@", &ids),
            ("@@MAX@@", &max.to_string()),
            ("@@LEVELS@@", levels),
        ],
    ))
}

async fn evt_snapshot(ctx: Ctx, args: Args) -> CapResult {
    let script = snapshot_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

pub fn crashes_script(args: &Args) -> String {
    let days = int_arg(args, "since_days", 30).clamp(1, 365);
    let max = int_arg(args, "max_events", 50).clamp(1, 500);
    fill(
        CRASHES_TEMPLATE,
        &[
            ("@@DAYS@@", &days.to_string()),
            ("@@MAX@@", &max.to_string()),
        ],
    )
}

async fn evt_crashes(ctx: Ctx, args: Args) -> CapResult {
    run_pwsh_json(&ctx.cancel, &crashes_script(&args))
        .await
        .map_err(HandlerError::from)
}

pub fn providers_script(args: &Args) -> Result<String, HandlerError> {
    let prefix = string_arg(args, "name_prefix", "").trim().to_string();
    if prefix.len() > 128 {
        return Err(HandlerError::plain("evt.providers: name_prefix too long"));
    }
    let mut max = int_arg(args, "max_results", 400);
    if max <= 0 {
        max = 400;
    }
    if max > 4000 {
        max = 4000;
    }
    Ok(fill(
        PROVIDERS_TEMPLATE,
        &[
            ("@@PREFIX@@", &ps_single_quote(&prefix)),
            ("@@MAX@@", &max.to_string()),
        ],
    ))
}

async fn evt_providers(ctx: Ctx, args: Args) -> CapResult {
    let script = providers_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn registers_the_event_log_kinds_with_labelled_specs() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(
            r.kinds(),
            vec!["evt.crashes", "evt.providers", "evt.snapshot"]
        );
        for spec in r.specs() {
            assert!(!spec.label.is_empty(), "{} has no label", spec.kind);
        }
        let crashes = r.spec_for("evt.crashes").unwrap();
        assert!(!crashes.risky && crashes.default_timeout_ms > 0);
    }

    #[test]
    fn evt_providers_rejects_over_long_prefix() {
        let err = providers_script(&args(json!({"name_prefix": "a".repeat(200)}))).unwrap_err();
        assert!(err.message.contains("too long"), "{}", err.message);
    }

    #[test]
    fn evt_providers_defaults_and_clamps() {
        let script = providers_script(&Args::new()).unwrap();
        assert!(script.contains("$max = 400"));
        let script =
            providers_script(&args(json!({"max_results": 99999, "name_prefix": " Mic "}))).unwrap();
        assert!(script.contains("$max = 4000"));
        assert!(script.contains("$prefix = 'Mic'"));
    }

    #[test]
    fn evt_snapshot_defaults_to_the_system_log() {
        let script = snapshot_script(&Args::new()).unwrap();
        assert!(script.contains("$channel = 'System'"));
        assert!(script.contains("AddMinutes(-60)"));
        assert!(script.contains("-MaxEvents 200"));
        assert!(script.contains("$provider = ''") && script.contains("$ids = @()"));
        assert!(!script.contains("@@"));
    }

    #[test]
    fn evt_snapshot_clamps_event_count() {
        let script =
            snapshot_script(&args(json!({"max_events": 10000, "since_minutes": 5}))).unwrap();
        assert!(script.contains("-MaxEvents 5000"));
        assert!(script.contains("AddMinutes(-5)"));
    }

    #[test]
    fn evt_snapshot_filters_by_level() {
        let script = snapshot_script(&args(json!({}))).unwrap();
        assert!(script.contains("$levels = @()"));
        let script = snapshot_script(&args(json!({"level": "Warning"}))).unwrap();
        assert!(script.contains("$levels = @(1,2,3)"));
        assert!(!script.contains("@@"));
        assert!(snapshot_script(&args(json!({"level": "loud"}))).is_err());
    }

    #[test]
    fn evt_snapshot_accepts_any_well_formed_channel() {
        for channel in [
            "Application",
            "Security",
            "Windows PowerShell",
            "Microsoft-Windows-Kernel-PnP/Configuration",
            "Microsoft-Windows-MF-FrameServer/Camera_FrameServer",
        ] {
            let script = snapshot_script(&args(json!({"channel": channel}))).unwrap();
            assert!(
                script.contains(&format!("$channel = '{channel}'")),
                "{channel}"
            );
        }
        for bad in ["System'; calc; '", "Sys$tem", "a\nb", "*", "..\\x"] {
            let err = snapshot_script(&args(json!({"channel": bad}))).unwrap_err();
            assert!(
                err.message.contains("not a valid event log name"),
                "{bad}: {}",
                err.message
            );
        }
        assert!(snapshot_script(&args(json!({"channel": "x".repeat(257)}))).is_err());
        let blank = snapshot_script(&args(json!({"channel": "  "}))).unwrap();
        assert!(blank.contains("$channel = 'System'"));
    }

    #[test]
    fn evt_snapshot_filters_by_provider_and_ids() {
        let script = snapshot_script(&args(
            json!({"provider": "Application Error", "event_id": " 1001, 1000,1000 "}),
        ))
        .unwrap();
        assert!(script.contains("$provider = 'Application Error'"));
        assert!(script.contains("$ids = @(1000,1001)"));
        assert!(snapshot_script(&args(json!({"provider": "x'y"}))).is_err());
        assert!(snapshot_script(&args(json!({"event_id": "41,abc"}))).is_err());
        assert!(snapshot_script(&args(json!({"event_id": "70000"}))).is_err());
        let many = (1..=21)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert!(snapshot_script(&args(json!({"event_id": many}))).is_err());
    }

    #[test]
    fn evt_crashes_defaults_and_clamps() {
        let script = crashes_script(&Args::new());
        assert!(script.contains("AddDays(-30)") && script.contains("$max = 50"));
        let script = crashes_script(&args(json!({"since_days": 9999, "max_events": 0})));
        assert!(script.contains("AddDays(-365)") && script.contains("$max = 1"));
        assert!(!script.contains("@@"));
        for needle in [
            "'Microsoft-Windows-Kernel-Power' 41",
            "'EventLog' 6008",
            "'Microsoft-Windows-WER-SystemErrorReporting' 1001",
            "'Application Error' 1000",
            "'Windows Error Reporting' 1001",
            "Minidump",
        ] {
            assert!(script.contains(needle), "{needle}");
        }
    }
}

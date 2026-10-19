use serde_json::Value;

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::pwsh::{json_script_handler, ps_single_quote, run_pwsh_json};

const INFO: &str = r##"Get-ComputerInfo | ConvertTo-Json -Compress -Depth 4"##;

const UPTIME: &str = r##"
$os = Get-CimInstance Win32_OperatingSystem
$boot = $os.LastBootUpTime
$now = Get-Date
$up = ($now - $boot).TotalSeconds
[ordered]@{
    boot_utc = $boot.ToUniversalTime().ToString("o")
    uptime_seconds = [int64]$up
    last_boot_local = $boot.ToString("o")
} | ConvertTo-Json -Compress
"##;

const HOTFIXES: &str = r##"
Get-HotFix |
    Sort-Object InstalledOn -Descending |
    Select-Object HotFixID,Description,InstalledBy,@{n='InstalledOn';e={ if ($_.InstalledOn) { $_.InstalledOn.ToString("o") } else { $null } }} |
    ConvertTo-Json -Compress -Depth 4
"##;

const ENV_TEMPLATE: &str = r##"
$scope = @@SCOPE@@
$prefix = @@PREFIX@@
$entries = New-Object System.Collections.Generic.List[object]
function Add-Scope($name, $target) {
    foreach ($entry in [Environment]::GetEnvironmentVariables($target).GetEnumerator()) {
        $key = [string]$entry.Key
        if ($prefix -and -not $key.ToLowerInvariant().StartsWith($prefix.ToLowerInvariant())) { return }
        $entries.Add([ordered]@{
            scope = $name
            name  = $key
            value = [string]$entry.Value
        }) | Out-Null
    }
}
if ($scope -eq 'all' -or $scope -eq 'machine') { Add-Scope 'machine' 'Machine' }
if ($scope -eq 'all' -or $scope -eq 'user')    { Add-Scope 'user'    'User' }
if ($scope -eq 'all' -or $scope -eq 'process') { Add-Scope 'process' 'Process' }
[ordered]@{
    scope   = $scope
    prefix  = $prefix
    count   = $entries.Count
    entries = @($entries | Sort-Object scope, name)
} | ConvertTo-Json -Compress -Depth 4
"##;

const USERS: &str = r##"
$logon = Get-CimInstance Win32_LoggedOnUser -ErrorAction SilentlyContinue |
    ForEach-Object {
        $acc = $_.Antecedent
        if ($acc -match 'Domain="([^"]+)",Name="([^"]+)"') {
            [pscustomobject]@{
                domain = $matches[1]
                name   = $matches[2]
            }
        }
    } | Sort-Object -Property name -Unique

$sessions = @()
try {
    $raw = (& query.exe session) 2>$null
    if ($raw -and $LASTEXITCODE -eq 0) {
        $idx = 0
        foreach ($line in $raw) {
            $idx++
            if ($idx -eq 1) { continue }
            if (-not $line.Trim()) { continue }
            $sessions += [ordered]@{
                raw = $line.Trim()
            }
        }
    }
} catch {}

[ordered]@{
    accounts = @($logon | Select-Object -First 50)
    terminal_sessions = @($sessions | Select-Object -First 50)
} | ConvertTo-Json -Compress -Depth 4
"##;

const TIMEZONE: &str = r##"
$tz = Get-TimeZone
$now = Get-Date
[ordered]@{
    id            = $tz.Id
    display_name  = $tz.DisplayName
    standard_name = $tz.StandardName
    base_utc_offset_minutes = [int]$tz.BaseUtcOffset.TotalMinutes
    supports_dst  = $tz.SupportsDaylightSavingTime
    is_dst_now    = $tz.IsDaylightSavingTime($now)
    local_now     = $now.ToString("o")
    utc_now       = $now.ToUniversalTime().ToString("o")
    culture       = (Get-Culture).Name
    ui_culture    = (Get-UICulture).Name
} | ConvertTo-Json -Compress
"##;

const REBOOT_REQUIRED: &str = r##"
$reasons = @()
if (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending') {
    $reasons += 'component_based_servicing'
}
if (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired') {
    $reasons += 'windows_update'
}
$sessionManager = Get-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager' -Name PendingFileRenameOperations -ErrorAction SilentlyContinue
if ($null -ne $sessionManager -and $sessionManager.PendingFileRenameOperations) {
    $reasons += 'pending_file_rename'
}
$computerName = $env:COMPUTERNAME
[ordered]@{
    required = ($reasons.Count -gt 0)
    reasons = @($reasons)
    computer_name = $computerName
} | ConvertTo-Json -Compress
"##;

const RESOURCES_TEMPLATE: &str = r##"
$top = @@TOP@@
$sampled = Get-Date
$os = Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue
$cpu = Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'" -ErrorAction SilentlyContinue
$page = Get-CimInstance Win32_PageFileUsage -ErrorAction SilentlyContinue |
    Select-Object @{n='name';e={[string]$_.Name}},
                  @{n='allocated_mb';e={[int]$_.AllocatedBaseSize}},
                  @{n='current_usage_mb';e={[int]$_.CurrentUsage}},
                  @{n='peak_usage_mb';e={[int]$_.PeakUsage}}

$procs = Get-Process -ErrorAction SilentlyContinue
$topByMemory = $procs |
    Sort-Object WorkingSet64 -Descending |
    Select-Object -First $top |
    Select-Object @{n='pid';e={[int]$_.Id}},
                  @{n='name';e={[string]$_.ProcessName}},
                  @{n='working_set_mb';e={[math]::Round($_.WorkingSet64 / 1MB, 1)}},
                  @{n='cpu_seconds';e={ if ($null -ne $_.CPU) { [math]::Round($_.CPU, 1) } else { $null } }}

$topByCpu = $procs |
    Where-Object { $null -ne $_.CPU } |
    Sort-Object CPU -Descending |
    Select-Object -First $top |
    Select-Object @{n='pid';e={[int]$_.Id}},
                  @{n='name';e={[string]$_.ProcessName}},
                  @{n='cpu_seconds';e={[math]::Round($_.CPU, 1)}},
                  @{n='working_set_mb';e={[math]::Round($_.WorkingSet64 / 1MB, 1)}}

$totalMB = if ($os) { [double]$os.TotalVisibleMemorySize / 1024 } else { $null }
$freeMB = if ($os) { [double]$os.FreePhysicalMemory / 1024 } else { $null }
$usedPct = $null
if ($null -ne $totalMB -and $totalMB -gt 0 -and $null -ne $freeMB) {
    $usedPct = [math]::Round((($totalMB - $freeMB) / $totalMB) * 100, 1)
}

[ordered]@{
    sampled_utc = $sampled.ToUniversalTime().ToString("o")
    top = $top
    cpu_percent = if ($cpu) { [int]$cpu.PercentProcessorTime } else { $null }
    memory_total_mb = if ($null -ne $totalMB) { [math]::Round($totalMB, 1) } else { $null }
    memory_free_mb = if ($null -ne $freeMB) { [math]::Round($freeMB, 1) } else { $null }
    memory_used_percent = $usedPct
    pagefiles = @($page)
    top_processes_by_memory = @($topByMemory)
    top_processes_by_cpu = @($topByCpu)
} | ConvertTo-Json -Compress -Depth 5
"##;

pub fn register(r: &mut Router) {
    r.register("sys.info", json_script_handler(INFO));
    r.register("sys.uptime", json_script_handler(UPTIME));
    r.register("sys.hotfixes", json_script_handler(HOTFIXES));
    r.register("sys.reboot-required", json_script_handler(REBOOT_REQUIRED));
    r.register("sys.env", handler(sys_env));
    r.register("sys.users", json_script_handler(USERS));
    r.register("sys.timezone", json_script_handler(TIMEZONE));
    r.register("sys.resources", handler(sys_resources));
}

fn string_arg(args: &Args, name: &str, default: &str) -> String {
    match args.get(name) {
        Some(Value::String(s)) => s.clone(),
        _ => default.to_string(),
    }
}

pub fn env_script(args: &Args) -> Result<String, HandlerError> {
    let scope = string_arg(args, "scope", "all").trim().to_lowercase();
    match scope.as_str() {
        "" | "all" | "machine" | "user" | "process" => {}
        _ => {
            return Err(HandlerError::plain(
                "sys.env: scope must be all, machine, user, or process",
            ))
        }
    }
    let scope = if scope.is_empty() {
        "all".to_string()
    } else {
        scope
    };

    let prefix = string_arg(args, "name_prefix", "").trim().to_string();
    if prefix.len() > 128 {
        return Err(HandlerError::plain("sys.env: name_prefix too long"));
    }

    Ok(ENV_TEMPLATE
        .replace("@@SCOPE@@", &ps_single_quote(&scope))
        .replace("@@PREFIX@@", &ps_single_quote(&prefix)))
}

async fn sys_env(ctx: Ctx, args: Args) -> CapResult {
    let script = env_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

pub fn resource_top_arg(args: &Args) -> i64 {
    let mut top = 10;
    if let Some(n) = args.get("top").and_then(Value::as_i64) {
        top = n;
    }
    if top <= 0 {
        top = 10;
    }
    top.min(50)
}

async fn sys_resources(ctx: Ctx, args: Args) -> CapResult {
    let script = RESOURCES_TEMPLATE.replace("@@TOP@@", &resource_top_arg(&args).to_string());
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn registers_the_sys_kinds() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(
            r.kinds(),
            vec![
                "sys.env",
                "sys.hotfixes",
                "sys.info",
                "sys.reboot-required",
                "sys.resources",
                "sys.timezone",
                "sys.uptime",
                "sys.users"
            ]
        );
    }

    #[test]
    fn sys_env_rejects_invalid_scope() {
        let err = env_script(&args(json!({"scope": "session"}))).unwrap_err();
        assert!(err.message.contains("scope must be"), "{}", err.message);
    }

    #[test]
    fn sys_env_normalizes_scope_and_quotes_prefix() {
        let script = env_script(&args(
            json!({"scope": " Machine ", "name_prefix": " O'Neil "}),
        ))
        .unwrap();
        assert!(script.contains("$scope = 'machine'"));
        assert!(script.contains("$prefix = 'O''Neil'"));
        assert!(!script.contains("@@"));
    }

    #[test]
    fn sys_env_defaults_to_all_scopes() {
        let script = env_script(&Args::new()).unwrap();
        assert!(script.contains("$scope = 'all'"));
        let script = env_script(&args(json!({"scope": ""}))).unwrap();
        assert!(script.contains("$scope = 'all'"));
    }

    #[test]
    fn sys_env_rejects_over_long_prefix() {
        let err = env_script(&args(json!({"name_prefix": "a".repeat(200)}))).unwrap_err();
        assert!(err.message.contains("too long"), "{}", err.message);
    }

    #[test]
    fn resource_top_arg_clamps() {
        assert_eq!(resource_top_arg(&args(json!({"top": 500}))), 50);
        assert_eq!(resource_top_arg(&args(json!({"top": 0}))), 10);
        assert_eq!(resource_top_arg(&args(json!({"top": 7}))), 7);
        assert_eq!(resource_top_arg(&Args::new()), 10);
    }

    #[test]
    fn resources_script_embeds_the_top_value() {
        let script = RESOURCES_TEMPLATE.replace("@@TOP@@", "12");
        assert!(script.contains("$top = 12"));
    }
}

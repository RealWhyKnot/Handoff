use serde_json::{json, Map, Value};

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::pwsh::ps_single_quote;
use super::pwsh_out::{json_script_handler, run_pwsh_json};

const INFO: &str = r##"
$os = Get-CimInstance Win32_OperatingSystem
$cs = Get-CimInstance Win32_ComputerSystem
$bios = Get-CimInstance Win32_BIOS
$cpus = @(Get-CimInstance Win32_Processor -Property Name,NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed,SocketDesignation)
$mem = @(Get-CimInstance Win32_PhysicalMemory -Property Capacity -ErrorAction SilentlyContinue)
$cv = Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -ErrorAction SilentlyContinue
$dg = Get-CimInstance -Namespace root\Microsoft\Windows\DeviceGuard -ClassName Win32_DeviceGuard -ErrorAction SilentlyContinue
function Pick($map, $v) { if ($null -eq $v) { return $null }; $k = [int]$v; if ($map.ContainsKey($k)) { $map[$k] } else { [string]$v } }
$domainRole = @{ 0 = 'StandaloneWorkstation'; 1 = 'MemberWorkstation'; 2 = 'StandaloneServer'; 3 = 'MemberServer'; 4 = 'BackupDomainController'; 5 = 'PrimaryDomainController' }
$pcType = @{ 0 = 'Unspecified'; 1 = 'Desktop'; 2 = 'Mobile'; 3 = 'Workstation'; 4 = 'EnterpriseServer'; 5 = 'SOHOServer'; 6 = 'AppliancePC'; 7 = 'PerformanceServer'; 8 = 'Slate' }
$productType = @{ 1 = 'WorkStation'; 2 = 'DomainController'; 3 = 'Server' }
$vbs = @{ 0 = 'Off'; 1 = 'EnabledNotRunning'; 2 = 'Running' }
$dgSvc = @{ 0 = 'None'; 1 = 'CredentialGuard'; 2 = 'HypervisorEnforcedCodeIntegrity'; 3 = 'SystemGuardSecureLaunch'; 4 = 'SmmFirmwareMeasurement'; 5 = 'KernelModeHardwareEnforcedStackProtection'; 6 = 'KernelModeHardwareEnforcedStackProtectionAudit'; 7 = 'HypervisorEnforcedPagingTranslation' }
$lang = $null
try { $lang = [Globalization.CultureInfo]::new([int]$os.OSLanguage).Name } catch {}
$installed = ($mem | Measure-Object -Property Capacity -Sum).Sum
$fw = switch ($env:firmware_type) { 'UEFI' { 'Uefi' } 'Legacy' { 'Bios' } default { $env:firmware_type } }
[ordered]@{
    WindowsProductName = [string]$os.Caption
    WindowsEditionId = [string]$cv.EditionID
    WindowsInstallationType = [string]$cv.InstallationType
    OSDisplayVersion = [string]$cv.DisplayVersion
    WindowsVersion = [string]$cv.ReleaseId
    WindowsBuildLabEx = [string]$cv.BuildLabEx
    WindowsUBR = if ($null -ne $cv.UBR) { [int]$cv.UBR } else { $null }
    WindowsRegisteredOwner = [string]$cv.RegisteredOwner
    WindowsRegisteredOrganization = [string]$cv.RegisteredOrganization
    WindowsProductId = [string]$cv.ProductId
    WindowsSystemRoot = [string]$cv.SystemRoot
    OsName = [string]$os.Caption
    OsVersion = [string]$os.Version
    OsBuildNumber = [string]$os.BuildNumber
    OsArchitecture = [string]$os.OSArchitecture
    OsProductType = Pick $productType $os.ProductType
    OsLanguage = $lang
    OsInstallDate = $os.InstallDate.ToUniversalTime().ToString("o")
    OsLastBootUpTime = $os.LastBootUpTime.ToUniversalTime().ToString("o")
    OsUptimeSeconds = [int64]((Get-Date) - $os.LastBootUpTime).TotalSeconds
    OsRegisteredUser = [string]$os.RegisteredUser
    OsSerialNumber = [string]$os.SerialNumber
    OsSystemDrive = [string]$os.SystemDrive
    OsWindowsDirectory = [string]$os.WindowsDirectory
    OsTotalVisibleMemorySize = [int64]$os.TotalVisibleMemorySize
    OsFreePhysicalMemory = [int64]$os.FreePhysicalMemory
    OsTotalVirtualMemorySize = [int64]$os.TotalVirtualMemorySize
    OsFreeVirtualMemory = [int64]$os.FreeVirtualMemory
    OsNumberOfProcesses = [int]$os.NumberOfProcesses
    OsNumberOfUsers = [int]$os.NumberOfUsers
    OsPortableOperatingSystem = [bool]$os.PortableOperatingSystem
    BiosManufacturer = [string]$bios.Manufacturer
    BiosVersion = [string]$bios.Version
    BiosSMBIOSBIOSVersion = [string]$bios.SMBIOSBIOSVersion
    BiosReleaseDate = if ($bios.ReleaseDate) { $bios.ReleaseDate.ToUniversalTime().ToString("o") } else { $null }
    BiosSerialNumber = [string]$bios.SerialNumber
    BiosFirmwareType = $fw
    CsName = [string]$cs.Name
    CsDNSHostName = [string]$cs.DNSHostName
    CsDomain = [string]$cs.Domain
    CsWorkgroup = [string]$cs.Workgroup
    CsPartOfDomain = [bool]$cs.PartOfDomain
    CsDomainRole = Pick $domainRole $cs.DomainRole
    CsManufacturer = [string]$cs.Manufacturer
    CsModel = [string]$cs.Model
    CsSystemFamily = [string]$cs.SystemFamily
    CsSystemSKUNumber = [string]$cs.SystemSKUNumber
    CsSystemType = [string]$cs.SystemType
    CsPCSystemType = Pick $pcType $cs.PCSystemType
    CsNumberOfProcessors = [int]$cs.NumberOfProcessors
    CsNumberOfLogicalProcessors = [int]$cs.NumberOfLogicalProcessors
    CsProcessors = @($cpus | ForEach-Object { [ordered]@{ Name = ([string]$_.Name).Trim(); NumberOfCores = [int]$_.NumberOfCores; NumberOfLogicalProcessors = [int]$_.NumberOfLogicalProcessors; MaxClockSpeed = [int]$_.MaxClockSpeed; SocketDesignation = [string]$_.SocketDesignation } })
    CsTotalPhysicalMemory = [int64]$cs.TotalPhysicalMemory
    CsPhysicallyInstalledMemory = if ($installed) { [int64]($installed / 1KB) } else { $null }
    CsUserName = [string]$cs.UserName
    CsPrimaryOwnerName = [string]$cs.PrimaryOwnerName
    CsBootupState = [string]$cs.BootupState
    CsHypervisorPresent = [bool]$cs.HypervisorPresent
    HyperVisorPresent = [bool]$cs.HypervisorPresent
    TimeZone = [TimeZoneInfo]::Local.DisplayName
    LogonServer = [string]$env:LOGONSERVER
    DeviceGuardVirtualizationBasedSecurityStatus = if ($dg) { Pick $vbs $dg.VirtualizationBasedSecurityStatus } else { $null }
    DeviceGuardSecurityServicesRunning = if ($dg) { ,@($dg.SecurityServicesRunning | ForEach-Object { Pick $dgSvc $_ }) } else { ,@() }
} | ConvertTo-Json -Compress -Depth 4
"##;

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

const POWER: &str = r##"
Add-Type -AssemblyName System.Windows.Forms
$status = [System.Windows.Forms.SystemInformation]::PowerStatus
$plans = foreach ($line in @(powercfg.exe /list)) {
    if ($line -match '([0-9a-fA-F]{8}-[0-9a-fA-F-]{27})\s+\((.+?)\)\s*(\*)?\s*$') {
        [ordered]@{
            name = $Matches[2]
            guid = $Matches[1].ToLowerInvariant()
            active = [bool]$Matches[3]
        }
    }
}
$active = @($plans | Where-Object { $_.active } | Select-Object -First 1)
[ordered]@{
    active_plan = if ($active.Count) { $active[0].name } else { $null }
    power_line = [string]$status.PowerLineStatus
    battery_percent = if ($status.BatteryChargeStatus -band 128) { $null } else { [int]($status.BatteryLifePercent * 100) }
    plans = @($plans)
} | ConvertTo-Json -Compress -Depth 4
"##;

const RESOURCES_TEMPLATE: &str = r##"
$top = @@TOP@@
$sampleMs = 1000
$sampled = Get-Date
$cpuQuery = "SELECT PercentProcessorTime,Timestamp_Sys100NS FROM Win32_PerfRawData_PerfOS_Processor WHERE Name='_Total'"
$c1 = Get-CimInstance -Query $cpuQuery -ErrorAction SilentlyContinue
$first = @{}
foreach ($p in Get-Process -ErrorAction SilentlyContinue) { if ($null -ne $p.CPU) { $first[$p.Id] = [double]$p.CPU } }
$clock = [Diagnostics.Stopwatch]::StartNew()
Start-Sleep -Milliseconds $sampleMs
$procs = @(Get-Process -ErrorAction SilentlyContinue)
$elapsed = $clock.Elapsed.TotalSeconds
$c2 = Get-CimInstance -Query $cpuQuery -ErrorAction SilentlyContinue
$cpuPct = $null
if ($c1 -and $c2) {
    $dt = [double]$c2.Timestamp_Sys100NS - [double]$c1.Timestamp_Sys100NS
    $didle = [double]$c2.PercentProcessorTime - [double]$c1.PercentProcessorTime
    if ($dt -gt 0) { $cpuPct = [math]::Round([math]::Min(100, [math]::Max(0, (1 - $didle / $dt) * 100)), 1) }
}
$cores = [Environment]::ProcessorCount
$os = Get-CimInstance Win32_OperatingSystem -Property TotalVisibleMemorySize,FreePhysicalMemory -ErrorAction SilentlyContinue
$page = Get-CimInstance Win32_PageFileUsage -ErrorAction SilentlyContinue |
    Select-Object @{n='name';e={[string]$_.Name}},
                  @{n='allocated_mb';e={[int]$_.AllocatedBaseSize}},
                  @{n='current_usage_mb';e={[int]$_.CurrentUsage}},
                  @{n='peak_usage_mb';e={[int]$_.PeakUsage}}
$rows = foreach ($p in $procs) {
    $pct = $null
    if ($null -ne $p.CPU -and $first.ContainsKey($p.Id) -and $elapsed -gt 0) {
        $pct = [math]::Round([math]::Max(0, ([double]$p.CPU - $first[$p.Id]) / $elapsed / $cores * 100), 1)
    }
    [pscustomobject]@{
        pid = [int]$p.Id
        name = [string]$p.ProcessName
        cpu_percent = $pct
        cpu_seconds = if ($null -ne $p.CPU) { [math]::Round($p.CPU, 1) } else { $null }
        working_set_mb = [math]::Round($p.WorkingSet64 / 1MB, 1)
        ws = [int64]$p.WorkingSet64
    }
}
$topByMemory = $rows | Sort-Object ws -Descending | Select-Object -First $top | Select-Object pid,name,working_set_mb,cpu_percent,cpu_seconds
$topByCpu = $rows | Where-Object { $null -ne $_.cpu_percent } | Sort-Object cpu_percent, cpu_seconds -Descending | Select-Object -First $top | Select-Object pid,name,cpu_percent,cpu_seconds,working_set_mb
$totalMB = if ($os) { [double]$os.TotalVisibleMemorySize / 1024 } else { $null }
$freeMB = if ($os) { [double]$os.FreePhysicalMemory / 1024 } else { $null }
$usedPct = $null
if ($null -ne $totalMB -and $totalMB -gt 0 -and $null -ne $freeMB) {
    $usedPct = [math]::Round((($totalMB - $freeMB) / $totalMB) * 100, 1)
}
[ordered]@{
    sampled_utc = $sampled.ToUniversalTime().ToString("o")
    top = $top
    cpu_percent = $cpuPct
    cpu_sample_ms = [int]($elapsed * 1000)
    logical_processors = $cores
    memory_total_mb = if ($null -ne $totalMB) { [math]::Round($totalMB, 1) } else { $null }
    memory_free_mb = if ($null -ne $freeMB) { [math]::Round($freeMB, 1) } else { $null }
    memory_used_percent = $usedPct
    pagefiles = @($page)
    top_processes_by_memory = @($topByMemory)
    top_processes_by_cpu = @($topByCpu)
} | ConvertTo-Json -Compress -Depth 5
"##;

const TIME_SYNC: &str = r##"
$lines = @(w32tm /query /status /verbose 2>&1 | ForEach-Object { [string]$_ })
$code = $LASTEXITCODE
$svc = Get-Service -Name W32Time -ErrorAction SilentlyContinue
$params = Get-ItemProperty -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Services\W32Time\Parameters' -ErrorAction SilentlyContinue
[ordered]@{
    exit_code = $code
    service = if ($svc) { [string]$svc.Status } else { 'NotInstalled' }
    service_start_type = if ($svc) { [string]$svc.StartType } else { $null }
    sync_type = if ($params) { [string]$params.Type } else { $null }
    configured_servers = if ($params) { [string]$params.NtpServer } else { $null }
    lines = $lines
} | ConvertTo-Json -Compress -Depth 3
"##;

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/sys.json"),
        vec![
            ("sys.info", json_script_handler(INFO)),
            ("sys.uptime", json_script_handler(UPTIME)),
            ("sys.hotfixes", json_script_handler(HOTFIXES)),
            ("sys.reboot-required", json_script_handler(REBOOT_REQUIRED)),
            ("sys.env", handler(sys_env)),
            ("sys.users", json_script_handler(USERS)),
            ("sys.timezone", json_script_handler(TIMEZONE)),
            ("sys.resources", handler(sys_resources)),
            ("sys.power", json_script_handler(POWER)),
            ("sys.time-sync", handler(sys_time_sync)),
        ],
    );
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

fn leading_number(s: &str) -> Option<f64> {
    let t = s.trim_start();
    let end = t
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && c == '-')))
        .map(|(i, _)| i)
        .unwrap_or(t.len());
    t[..end].parse::<f64>().ok().filter(|f| f.is_finite())
}

fn in_parens(s: &str) -> Option<String> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    (close > open).then(|| s[open + 1..close].trim().to_string())
}

fn num_value(v: Option<f64>) -> Value {
    match v {
        Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => json!(f as i64),
        Some(f) => json!(f),
        None => Value::Null,
    }
}

pub fn parse_w32tm_status(lines: &[String], now_ms: i64) -> Map<String, Value> {
    let mut out = Map::new();
    for line in lines {
        let Some(i) = line.find(':') else { continue };
        let key = line[..i].trim().to_ascii_lowercase();
        let value = line[i + 1..].trim();
        match key.as_str() {
            "leap indicator" => {
                out.insert("leap_indicator".into(), json!(value));
            }
            "stratum" => {
                out.insert("stratum".into(), num_value(leading_number(value)));
            }
            "root delay" => {
                out.insert(
                    "root_delay_seconds".into(),
                    num_value(leading_number(value)),
                );
            }
            "root dispersion" => {
                out.insert(
                    "root_dispersion_seconds".into(),
                    num_value(leading_number(value)),
                );
            }
            "referenceid" => {
                out.insert("reference_id".into(), json!(value));
            }
            "last successful sync time" => {
                out.insert("last_sync_local".into(), json!(value));
            }
            "source" => {
                let name = value.split(',').next().unwrap_or(value).trim();
                out.insert("source".into(), json!(name));
                let lower = name.to_ascii_lowercase();
                out.insert(
                    "using_local_clock".into(),
                    json!(lower.contains("local cmos clock") || lower.contains("free-running")),
                );
            }
            "poll interval" => {
                let secs = in_parens(value).and_then(|p| leading_number(&p));
                out.insert("poll_interval_seconds".into(), num_value(secs));
            }
            "phase offset" => {
                out.insert(
                    "phase_offset_seconds".into(),
                    num_value(leading_number(value)),
                );
            }
            "state machine" => {
                out.insert(
                    "state".into(),
                    json!(in_parens(value).unwrap_or_else(|| value.to_string())),
                );
            }
            "last sync error" => {
                out.insert(
                    "last_sync_error_code".into(),
                    num_value(leading_number(value)),
                );
                out.insert("last_sync_error".into(), json!(in_parens(value)));
            }
            "time since last good sync time" => {
                let secs = leading_number(value);
                out.insert(
                    "seconds_since_last_good_sync".into(),
                    num_value(secs.map(f64::round)),
                );
                if let Some(s) = secs {
                    let at = now_ms - (s * 1000.0) as i64;
                    out.insert(
                        "last_good_sync_utc".into(),
                        json!(crate::util::format_utc_ms(at)),
                    );
                }
            }
            _ => {}
        }
    }
    out
}

pub fn shape_time_sync(raw: &Value, now_ms: i64) -> Value {
    let lines: Vec<String> = raw
        .get("lines")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let code = raw.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
    let mut out = Map::new();
    for key in [
        "service",
        "service_start_type",
        "sync_type",
        "configured_servers",
    ] {
        out.insert(key.into(), raw.get(key).cloned().unwrap_or(Value::Null));
    }
    if code == 0 {
        out.extend(parse_w32tm_status(&lines, now_ms));
        out.insert("error".into(), Value::Null);
    } else {
        let text = lines
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        out.insert("error".into(), json!(text));
    }
    Value::Object(out)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn sys_time_sync(ctx: Ctx, _args: Args) -> CapResult {
    let raw = run_pwsh_json(&ctx.cancel, TIME_SYNC)
        .await
        .map_err(HandlerError::from)?;
    Ok(shape_time_sync(&raw, now_ms()))
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
                "sys.power",
                "sys.reboot-required",
                "sys.resources",
                "sys.time-sync",
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

    const W32TM: &str = "Leap Indicator: 0(no warning)
Stratum: 5 (secondary reference - syncd by (S)NTP)
Precision: -23 (119.209ns per tick)
Root Delay: 0.0407987s
Root Dispersion: 0.2248368s
ReferenceId: 0x287706E4 (source IP:  40.119.6.228)
Last Successful Sync Time: 10/18/2026 10:36:14 PM
Source: time.windows.com,0x9
Poll Interval: 12 (4096s)

Phase Offset: -0.0000346s
ClockRate: 0.0156249s
State Machine: 2 (Sync)
Time Source Flags: 0 (None)
Server Role: 0 (None)
Last Sync Error: 2 (The computer did not resync because only stale time data was available.)
Time since Last Good Sync Time: 15080.9746700s
";

    #[test]
    fn w32tm_status_is_parsed() {
        let lines: Vec<String> = W32TM.lines().map(str::to_string).collect();
        let now = 1_792_400_000_000;
        let got = shape_time_sync(
            &json!({"exit_code": 0, "service": "Running", "lines": lines}),
            now,
        );
        assert_eq!(got["service"], "Running");
        assert_eq!(got["stratum"], 5);
        assert_eq!(got["source"], "time.windows.com");
        assert_eq!(got["using_local_clock"], false);
        assert_eq!(got["poll_interval_seconds"], 4096);
        assert_eq!(got["root_delay_seconds"], 0.0407987);
        assert_eq!(got["phase_offset_seconds"], -0.0000346);
        assert_eq!(got["state"], "Sync");
        assert_eq!(got["last_sync_error_code"], 2);
        assert_eq!(
            got["last_sync_error"],
            "The computer did not resync because only stale time data was available."
        );
        assert_eq!(got["seconds_since_last_good_sync"], 15081);
        assert_eq!(
            got["last_good_sync_utc"],
            json!(crate::util::format_utc_ms(now - 15_080_974))
        );
        assert_eq!(got["reference_id"], "0x287706E4 (source IP:  40.119.6.228)");
        assert!(got["error"].is_null());
    }

    #[test]
    fn w32tm_failure_is_reported_as_text() {
        let raw = json!({
            "exit_code": -2147023838,
            "service": "Stopped",
            "lines": ["The following error occurred: The service has not been started. (0x80070426)"],
        });
        let got = shape_time_sync(&raw, 0);
        assert_eq!(got["service"], "Stopped");
        assert_eq!(
            got["error"],
            "The following error occurred: The service has not been started. (0x80070426)"
        );
        assert!(got.get("stratum").is_none());
        let local = parse_w32tm_status(&["Source: Local CMOS Clock".to_string()], 0);
        assert_eq!(local["using_local_clock"], true);
    }

    #[test]
    fn info_and_resources_scripts_avoid_slow_queries() {
        assert!(!INFO.contains("Get-ComputerInfo"));
        assert!(INFO.contains("Win32_Processor -Property"));
        assert!(RESOURCES_TEMPLATE.contains("Win32_PerfRawData_PerfOS_Processor"));
        assert!(!RESOURCES_TEMPLATE.contains("PerfFormattedData"));
    }
}

use crate::dispatch::Router;

use super::pwsh_out::json_script_handler;

const CPU: &str = r##"Get-CimInstance Win32_Processor -Property Name,NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed,Manufacturer | Select-Object @{n='Name';e={([string]$_.Name).Trim()}},NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed,Manufacturer | ConvertTo-Json -Compress"##;

const RAM: &str = r##"Get-CimInstance Win32_PhysicalMemory | Select-Object DeviceLocator,@{n='SizeGB';e={[math]::Round($_.Capacity/1GB,2)}},Speed,Manufacturer,PartNumber | ConvertTo-Json -Compress"##;

const USB: &str = r##"Get-PnpDevice -PresentOnly | Where-Object {$_.InstanceId -like 'USB\*'} | Select-Object InstanceId,FriendlyName,Status,Class,Manufacturer | ConvertTo-Json -Compress"##;

const DISKS: &str = r##"
$disks = Get-PhysicalDisk | Select-Object DeviceId,FriendlyName,SerialNumber,@{n='SizeGB';e={[math]::Round($_.Size/1GB,2)}},MediaType,HealthStatus,OperationalStatus,BusType
$disks | ConvertTo-Json -Compress
"##;

const GPU: &str = r##"
function Mem-Bytes($v) {
    if ($null -eq $v) { return $null }
    if ($v -is [byte[]]) {
        if ($v.Length -ge 8) { return [BitConverter]::ToInt64($v, 0) }
        if ($v.Length -ge 4) { return [int64][BitConverter]::ToUInt32($v, 0) }
        return $null
    }
    [int64]$v
}
Get-CimInstance Win32_VideoController | ForEach-Object {
    $mem = $null
    $source = $null
    try {
        $drv = (Get-ItemProperty -LiteralPath ('HKLM:\SYSTEM\CurrentControlSet\Enum\' + $_.PNPDeviceID) -Name Driver -ErrorAction Stop).Driver
        $cls = Get-ItemProperty -LiteralPath ('HKLM:\SYSTEM\CurrentControlSet\Control\Class\' + $drv) -ErrorAction Stop
        $mem = Mem-Bytes $cls.'HardwareInformation.qwMemorySize'
        if (-not $mem) { $mem = Mem-Bytes $cls.'HardwareInformation.MemorySize' }
        if ($mem) { $source = 'registry' }
    } catch {}
    if (-not $mem -and $_.AdapterRAM) {
        $mem = [int64]$_.AdapterRAM
        $source = 'wmi'
    }
    [ordered]@{
        Name = [string]$_.Name
        AdapterRAM = $mem
        AdapterRAMGB = if ($mem) { [math]::Round($mem / 1GB, 2) } else { $null }
        AdapterRAMSource = $source
        DriverVersion = [string]$_.DriverVersion
        DriverDate = $_.DriverDate
        VideoProcessor = $_.VideoProcessor
        CurrentResolution = if ($_.CurrentHorizontalResolution) { '{0}x{1}' -f $_.CurrentHorizontalResolution, $_.CurrentVerticalResolution } else { $null }
        RefreshRateHz = $_.CurrentRefreshRate
        Status = [string]$_.Status
    }
} | ConvertTo-Json -Compress
"##;

const DISK_HEALTH: &str = r##"
$disks = @(Get-PhysicalDisk -ErrorAction SilentlyContinue | Sort-Object { [int]$_.DeviceId })
$denied = 'Only an administrator can read these counters.'
$rows = @(foreach ($d in $disks) {
    $rel = $null
    $relError = $null
    try {
        $r = $d | Get-StorageReliabilityCounter -ErrorAction Stop
        if ($r) {
            $rel = [ordered]@{
                temperature_c = $r.Temperature
                temperature_max_c = $r.TemperatureMax
                wear_percent = $r.Wear
                power_on_hours = $r.PowerOnHours
                start_stop_count = $r.StartStopCycleCount
                read_errors_total = $r.ReadErrorsTotal
                read_errors_uncorrected = $r.ReadErrorsUncorrected
                write_errors_total = $r.WriteErrorsTotal
                write_errors_uncorrected = $r.WriteErrorsUncorrected
                read_latency_max_ms = $r.ReadLatencyMax
                write_latency_max_ms = $r.WriteLatencyMax
            }
        } else {
            $relError = 'This disk does not report health counters.'
        }
    } catch {
        $relError = if ($_.CategoryInfo.Category -eq 'PermissionDenied' -or $_.Exception.HResult -eq -2147024891) { $denied } else { $_.Exception.Message }
    }
    [ordered]@{
        number = [int]$d.DeviceId
        name = [string]$d.FriendlyName
        serial = ([string]$d.SerialNumber).Trim()
        media_type = [string]$d.MediaType
        bus_type = [string]$d.BusType
        size_gb = [math]::Round($d.Size / 1GB, 1)
        health = [string]$d.HealthStatus
        operational_status = (@($d.OperationalStatus) | ForEach-Object { [string]$_ }) -join ', '
        firmware = [string]$d.FirmwareVersion
        usage = [string]$d.Usage
        reliability = $rel
        reliability_error = $relError
    }
})
[ordered]@{
    count = $rows.Count
    unhealthy = @($rows | Where-Object { $_.health -ne 'Healthy' }).Count
    disks = $rows
} | ConvertTo-Json -Compress -Depth 4
"##;

const BATTERY: &str = r##"
Add-Type -AssemblyName System.Windows.Forms
$power = [System.Windows.Forms.SystemInformation]::PowerStatus
$batteries = @(Get-CimInstance Win32_Battery -ErrorAction SilentlyContinue)
$static = @(Get-CimInstance -Namespace root\wmi -ClassName BatteryStaticData -ErrorAction SilentlyContinue)
$full = @(Get-CimInstance -Namespace root\wmi -ClassName BatteryFullChargedCapacity -ErrorAction SilentlyContinue)
$names = @{1='discharging';2='on AC';3='fully charged';4='low';5='critical';6='charging';7='charging';8='charging';9='charging';11='partially charged'}
$rows = for ($i = 0; $i -lt $batteries.Count; $i++) {
    $b = $batteries[$i]
    $design = if ($static.Count -gt $i) { [int64]$static[$i].DesignedCapacity } else { $null }
    $fullCap = if ($full.Count -gt $i) { [int64]$full[$i].FullChargedCapacity } else { $null }
    [ordered]@{
        name = [string]$b.Name
        charge_percent = [int]$b.EstimatedChargeRemaining
        status = $names[[int]$b.BatteryStatus]
        minutes_left = if ($b.EstimatedRunTime -and $b.EstimatedRunTime -lt 71582788) { [int]$b.EstimatedRunTime } else { $null }
        design_capacity_mwh = $design
        full_charge_capacity_mwh = $fullCap
        health_percent = if ($design -and $fullCap) { [math]::Round(100 * $fullCap / $design, 1) } else { $null }
    }
}
[ordered]@{
    present = $batteries.Count -gt 0
    power_line = [string]$power.PowerLineStatus
    batteries = @($rows)
} | ConvertTo-Json -Compress -Depth 4
"##;

const DISPLAYS: &str = r##"
Add-Type -Namespace HandoffDpi -Name Native -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();'
[void][HandoffDpi.Native]::SetProcessDPIAware()
Add-Type -AssemblyName System.Windows.Forms
function Read-WmiText($codes) {
    if (-not $codes) { return $null }
    $chars = @($codes | Where-Object { $_ -ne 0 } | ForEach-Object { [char]$_ })
    (-join $chars).Trim()
}
$screens = foreach ($s in [System.Windows.Forms.Screen]::AllScreens) {
    [ordered]@{
        device = [string]$s.DeviceName
        primary = [bool]$s.Primary
        width = [int]$s.Bounds.Width
        height = [int]$s.Bounds.Height
        x = [int]$s.Bounds.X
        y = [int]$s.Bounds.Y
        bits_per_pixel = [int]$s.BitsPerPixel
    }
}
$monitors = foreach ($m in @(Get-CimInstance -Namespace root\wmi -ClassName WmiMonitorID -ErrorAction SilentlyContinue)) {
    [ordered]@{
        manufacturer = Read-WmiText $m.ManufacturerName
        model = Read-WmiText $m.UserFriendlyName
        serial = Read-WmiText $m.SerialNumberID
        year = [int]$m.YearOfManufacture
        active = [bool]$m.Active
    }
}
$adapters = foreach ($v in @(Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue)) {
    [ordered]@{
        name = [string]$v.Name
        width = $v.CurrentHorizontalResolution
        height = $v.CurrentVerticalResolution
        refresh_hz = $v.CurrentRefreshRate
    }
}
[ordered]@{
    screens = @($screens)
    monitors = @($monitors)
    adapters = @($adapters)
} | ConvertTo-Json -Compress -Depth 4
"##;

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/hw.json"),
        vec![
            ("hw.cpu", json_script_handler(CPU)),
            ("hw.ram", json_script_handler(RAM)),
            ("hw.usb", json_script_handler(USB)),
            ("hw.disks", json_script_handler(DISKS)),
            ("hw.gpu", json_script_handler(GPU)),
            ("hw.battery", json_script_handler(BATTERY)),
            ("hw.displays", json_script_handler(DISPLAYS)),
            ("hw.disk-health", json_script_handler(DISK_HEALTH)),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_the_hardware_inventory_kinds() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(
            r.kinds(),
            vec![
                "hw.battery",
                "hw.cpu",
                "hw.disk-health",
                "hw.disks",
                "hw.displays",
                "hw.gpu",
                "hw.ram",
                "hw.usb"
            ]
        );
        for spec in r.specs() {
            assert!(!spec.label.is_empty(), "{} has no label", spec.kind);
        }
        let health = r.spec_for("hw.disk-health").unwrap();
        assert!(!health.risky && health.default_timeout_ms > 0);
    }

    #[test]
    fn scripts_end_in_json_output() {
        for script in [CPU, RAM, USB, DISKS, GPU, BATTERY, DISPLAYS, DISK_HEALTH] {
            assert!(script.contains("ConvertTo-Json -Compress"));
        }
        assert!(CPU.contains("Win32_Processor -Property"));
        assert!(GPU.contains("HardwareInformation.qwMemorySize"));
        assert!(DISK_HEALTH.contains("Get-StorageReliabilityCounter"));
    }
}

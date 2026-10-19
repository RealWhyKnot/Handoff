use crate::dispatch::Router;

use super::pwsh::json_script_handler;

const CPU: &str = r##"Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed,Manufacturer | ConvertTo-Json -Compress"##;

const RAM: &str = r##"Get-CimInstance Win32_PhysicalMemory | Select-Object DeviceLocator,@{n='SizeGB';e={[math]::Round($_.Capacity/1GB,2)}},Speed,Manufacturer,PartNumber | ConvertTo-Json -Compress"##;

const USB: &str = r##"Get-PnpDevice -PresentOnly | Where-Object {$_.InstanceId -like 'USB\*'} | Select-Object InstanceId,FriendlyName,Status,Class,Manufacturer | ConvertTo-Json -Compress"##;

const DISKS: &str = r##"
$disks = Get-PhysicalDisk | Select-Object DeviceId,FriendlyName,SerialNumber,@{n='SizeGB';e={[math]::Round($_.Size/1GB,2)}},MediaType,HealthStatus,OperationalStatus,BusType
$disks | ConvertTo-Json -Compress
"##;

const GPU: &str = r##"Get-CimInstance Win32_VideoController | Select-Object Name,AdapterRAM,DriverVersion,VideoProcessor,Status | ConvertTo-Json -Compress"##;

pub fn register(r: &mut Router) {
    r.register("hw.cpu", json_script_handler(CPU));
    r.register("hw.ram", json_script_handler(RAM));
    r.register("hw.usb", json_script_handler(USB));
    r.register("hw.disks", json_script_handler(DISKS));
    r.register("hw.gpu", json_script_handler(GPU));
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
            vec!["hw.cpu", "hw.disks", "hw.gpu", "hw.ram", "hw.usb"]
        );
        for spec in r.specs() {
            assert!(spec.params.is_empty() && spec.label.is_empty());
        }
    }

    #[test]
    fn scripts_end_in_json_output() {
        for script in [CPU, RAM, USB, DISKS, GPU] {
            assert!(script.contains("ConvertTo-Json -Compress"));
        }
    }
}

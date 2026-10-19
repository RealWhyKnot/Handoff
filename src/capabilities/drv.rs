use crate::dispatch::Router;

use super::pwsh::json_script_handler;

const LIST: &str = r##"Get-CimInstance Win32_PnpSignedDriver | Select-Object DeviceName,DeviceClass,DriverVersion,DriverDate,Manufacturer,Signer,InfName | ConvertTo-Json -Compress"##;

pub fn register(r: &mut Router) {
    r.register("drv.list", json_script_handler(LIST));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_drv_list() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["drv.list"]);
    }
}

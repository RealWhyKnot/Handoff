use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::proc::{bool_arg, fill};
use super::pwsh_out::{json_script_handler, run_pwsh_json};

const LIST: &str = r##"Get-CimInstance Win32_PnpSignedDriver | Select-Object DeviceName,DeviceClass,DriverVersion,DriverDate,Manufacturer,Signer,InfName | ConvertTo-Json -Compress"##;

const PROBLEMS_TEMPLATE: &str = r##"
$includeAbsent = @@ABSENT@@
$meaning = @{
    1 = 'Not configured correctly'
    3 = 'Driver may be damaged, or the system is low on memory'
    10 = 'Device cannot start'
    12 = 'Not enough free resources'
    14 = 'Needs a restart to work'
    16 = 'Windows cannot identify all of its resources'
    18 = 'Drivers need to be reinstalled'
    19 = 'Registry settings are incomplete or damaged'
    21 = 'Windows is removing this device'
    22 = 'Device is disabled'
    24 = 'Device is missing, not working, or has no driver'
    28 = 'No driver installed'
    29 = 'Disabled by the firmware'
    31 = 'Windows cannot load the driver'
    32 = 'Driver service is disabled'
    33 = 'Windows cannot work out which resources it needs'
    34 = 'Device needs manual configuration'
    35 = 'Firmware lacks the information to configure it'
    36 = 'Interrupt conflict'
    37 = 'Driver failed to start'
    38 = 'An older copy of the driver is still loaded'
    39 = 'Driver is damaged or missing'
    40 = 'Driver service registry entry is missing or wrong'
    41 = 'Driver loaded but cannot find the device'
    42 = 'Duplicate device'
    43 = 'Device reported a problem and was stopped'
    44 = 'An app or service shut the device down'
    45 = 'Not connected right now'
    46 = 'Windows is shutting down'
    47 = 'Ready for safe removal'
    48 = 'Driver is blocked because of known problems'
    49 = 'System registry hive is too large'
    50 = 'Windows cannot apply all of its properties'
    51 = 'Waiting on another device'
    52 = 'Driver signature cannot be verified'
    53 = 'Reserved for the kernel debugger'
    54 = 'Device failed and is resetting'
    56 = 'Still being set up'
}
$devices = if ($includeAbsent) { Get-PnpDevice -ErrorAction SilentlyContinue } else { Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue }
$rows = @($devices | Where-Object { $_.Status -ne 'OK' } | Sort-Object Class, FriendlyName | ForEach-Object {
    $code = [int]$_.ConfigManagerErrorCode
    [ordered]@{
        name = [string]$_.FriendlyName
        class = [string]$_.Class
        status = [string]$_.Status
        present = [bool]$_.Present
        problem_code = $code
        problem = [string]$_.Problem
        meaning = if ($meaning.ContainsKey($code)) { $meaning[$code] } else { $null }
        manufacturer = [string]$_.Manufacturer
        instance_id = [string]$_.InstanceId
    }
})
[ordered]@{
    include_absent = $includeAbsent
    count = $rows.Count
    devices = $rows
} | ConvertTo-Json -Compress -Depth 4
"##;

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/drv.json"),
        vec![
            ("drv.list", json_script_handler(LIST)),
            ("drv.problems", handler(drv_problems)),
        ],
    );
}

pub fn problems_script(args: &Args) -> String {
    let absent = if bool_arg(args, "include_absent", false) {
        "$true"
    } else {
        "$false"
    };
    fill(PROBLEMS_TEMPLATE, &[("@@ABSENT@@", absent)])
}

async fn drv_problems(ctx: Ctx, args: Args) -> CapResult {
    run_pwsh_json(&ctx.cancel, &problems_script(&args))
        .await
        .map_err(HandlerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registers_the_driver_kinds() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["drv.list", "drv.problems"]);
        let spec = r.spec_for("drv.problems").unwrap();
        assert!(!spec.risky && spec.default_timeout_ms > 0);
    }

    #[test]
    fn problems_script_defaults_to_present_devices() {
        let script = problems_script(&Args::new());
        assert!(script.contains("$includeAbsent = $false"));
        let a = json!({"include_absent": true})
            .as_object()
            .cloned()
            .unwrap();
        assert!(problems_script(&a).contains("$includeAbsent = $true"));
        assert!(!script.contains("@@"));
        assert!(script.contains("52 = 'Driver signature cannot be verified'"));
    }
}

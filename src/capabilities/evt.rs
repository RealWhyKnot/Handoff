use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::proc::{fill, go_quote, int_arg, string_arg};
use super::pwsh::{ps_single_quote, run_pwsh_json};

const SNAPSHOT_TEMPLATE: &str = r##"
$start = (Get-Date).AddMinutes(-@@SINCE@@)
Get-WinEvent -FilterHashtable @{ LogName = '@@CHANNEL@@'; StartTime = $start } -MaxEvents @@MAX@@ -ErrorAction SilentlyContinue |
    Select-Object @{n='time';e={$_.TimeCreated.ToUniversalTime().ToString("o")}},LevelDisplayName,Id,ProviderName,Message |
    ConvertTo-Json -Compress -Depth 4
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

const ALLOWED_CHANNELS: [&str; 5] = [
    "System",
    "Application",
    "Setup",
    "Microsoft-Windows-Kernel-PnP/Configuration",
    "Microsoft-Windows-USB-USBHUB3-Analytic",
];

pub fn register(r: &mut Router) {
    r.register("evt.snapshot", handler(evt_snapshot));
    r.register("evt.providers", handler(evt_providers));
}

pub fn snapshot_script(args: &Args) -> Result<String, HandlerError> {
    let channel = string_arg(args, "channel", "System");
    let mut max = int_arg(args, "max_events", 200);
    if max <= 0 {
        max = 200;
    }
    if max > 5000 {
        max = 5000;
    }
    let mut since = int_arg(args, "since_minutes", 60);
    if since <= 0 {
        since = 60;
    }
    if !ALLOWED_CHANNELS.contains(&channel.as_str()) {
        return Err(HandlerError::plain(format!(
            "channel {} not in evt.snapshot allowlist",
            go_quote(&channel)
        )));
    }
    Ok(fill(
        SNAPSHOT_TEMPLATE,
        &[
            ("@@SINCE@@", &since.to_string()),
            ("@@CHANNEL@@", &channel),
            ("@@MAX@@", &max.to_string()),
        ],
    ))
}

async fn evt_snapshot(ctx: Ctx, args: Args) -> CapResult {
    let script = snapshot_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
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
    fn registers_the_event_log_kinds_as_plain_specs() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["evt.providers", "evt.snapshot"]);
        for spec in r.specs() {
            assert!(spec.params.is_empty() && spec.label.is_empty());
        }
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
        assert!(script.contains("LogName = 'System'"));
        assert!(script.contains("AddMinutes(-60)"));
        assert!(script.contains("-MaxEvents 200"));
    }

    #[test]
    fn evt_snapshot_clamps_event_count() {
        let script =
            snapshot_script(&args(json!({"max_events": 10000, "since_minutes": 5}))).unwrap();
        assert!(script.contains("-MaxEvents 5000"));
        assert!(script.contains("AddMinutes(-5)"));
    }

    #[test]
    fn evt_snapshot_only_allows_the_known_channels() {
        let err = snapshot_script(&args(json!({"channel": "Security"}))).unwrap_err();
        assert_eq!(
            err.message,
            "channel \"Security\" not in evt.snapshot allowlist"
        );
        let err = snapshot_script(&args(json!({"channel": "System'; calc; '"}))).unwrap_err();
        assert!(err.message.contains("allowlist"), "{}", err.message);
        for channel in [
            "Application",
            "Setup",
            "Microsoft-Windows-Kernel-PnP/Configuration",
        ] {
            assert!(snapshot_script(&args(json!({"channel": channel}))).is_ok());
        }
    }
}

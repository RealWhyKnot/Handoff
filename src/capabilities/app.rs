use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::proc::{fill, int_arg, load_specs, string_arg};
use super::pwsh::{ps_single_quote, run_pwsh_json};

const TEMPLATE: &str = r##"
$prefix = @@PREFIX@@
$max = @@MAX@@

$paths = @(
    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*'
)
$win32 = foreach ($p in $paths) {
    Get-ItemProperty $p -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName } |
        ForEach-Object {
            [ordered]@{
                kind        = 'win32'
                name        = [string]$_.DisplayName
                version     = [string]$_.DisplayVersion
                publisher   = [string]$_.Publisher
                install_date = [string]$_.InstallDate
                install_loc  = [string]$_.InstallLocation
                uninstall    = [string]$_.UninstallString
            }
        }
}

$appx = @()
try {
    $appx = Get-AppxPackage -AllUsers:$false -ErrorAction SilentlyContinue |
        ForEach-Object {
            [ordered]@{
                kind        = 'appx'
                name        = [string]$_.Name
                version     = [string]$_.Version
                publisher   = [string]$_.Publisher
                install_loc = [string]$_.InstallLocation
                family_name = [string]$_.PackageFamilyName
            }
        }
} catch {}

$all = @($win32 + $appx)
if ($prefix) {
    $needle = $prefix.ToLowerInvariant()
    $all = $all | Where-Object { $_.name -and $_.name.ToLowerInvariant().StartsWith($needle) }
}
$all = $all | Sort-Object @{Expression='kind'},@{Expression='name'} | Select-Object -First $max

[ordered]@{
    prefix  = $prefix
    count   = @($all).Count
    max     = $max
    entries = @($all)
} | ConvertTo-Json -Compress -Depth 4
"##;

pub fn register(r: &mut Router) {
    for spec in load_specs(include_str!("specs/app.json")) {
        r.register_spec(spec, handler(app_list));
    }
}

pub fn list_script(args: &Args) -> Result<String, HandlerError> {
    let prefix = string_arg(args, "name_prefix", "").trim().to_string();
    let mut max = int_arg(args, "limit", 300);
    if prefix.len() > 128 {
        return Err(HandlerError::plain("app.list: name_prefix too long"));
    }
    if max <= 0 {
        max = 300;
    }
    if max > 5000 {
        max = 5000;
    }
    Ok(fill(
        TEMPLATE,
        &[
            ("@@PREFIX@@", &ps_single_quote(&prefix)),
            ("@@MAX@@", &max.to_string()),
        ],
    ))
}

async fn app_list(ctx: Ctx, args: Args) -> CapResult {
    let script = list_script(&args)?;
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
    fn registers_app_list() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["app.list"]);
        assert_eq!(r.spec_for("app.list").unwrap().params.len(), 2);
    }

    #[test]
    fn app_list_rejects_over_long_prefix() {
        let err = list_script(&args(json!({"name_prefix": "a".repeat(200)}))).unwrap_err();
        assert!(err.message.contains("too long"), "{}", err.message);
    }

    #[test]
    fn app_list_quotes_the_prefix_and_clamps_the_limit() {
        let script =
            list_script(&args(json!({"name_prefix": " O'Neil ", "limit": 99999}))).unwrap();
        assert!(script.contains("$prefix = 'O''Neil'"));
        assert!(script.contains("$max = 5000"));
        let script = list_script(&args(json!({"limit": 0}))).unwrap();
        assert!(script.contains("$max = 300"));
        assert!(!script.contains("@@"));
    }
}

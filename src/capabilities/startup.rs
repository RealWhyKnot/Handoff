use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::proc::{fill, int_arg, load_specs};
use super::pwsh_out::run_pwsh_json;

const TEMPLATE: &str = r##"
$max = @@MAX@@
$entries = Get-CimInstance Win32_StartupCommand -ErrorAction SilentlyContinue |
    Sort-Object Location, Name |
    Select-Object -First $max |
    ForEach-Object {
        [ordered]@{
            name = [string]$_.Name
            command = [string]$_.Command
            location = [string]$_.Location
            user = [string]$_.User
            user_sid = [string]$_.UserSID
            description = [string]$_.Description
        }
    }

[ordered]@{
    max = $max
    count = @($entries).Count
    entries = @($entries)
} | ConvertTo-Json -Compress -Depth 4
"##;

pub fn register(r: &mut Router) {
    for spec in load_specs(include_str!("specs/startup.json")) {
        r.register_spec(spec, handler(startup_list));
    }
}

pub fn max_results_arg(args: &Args) -> i64 {
    let mut max = int_arg(args, "limit", 300);
    if max <= 0 {
        max = 300;
    }
    if max > 2000 {
        max = 2000;
    }
    max
}

async fn startup_list(ctx: Ctx, args: Args) -> CapResult {
    let script = fill(
        TEMPLATE,
        &[("@@MAX@@", &max_results_arg(&args).to_string())],
    );
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
    fn registers_startup_list() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["startup.list"]);
    }

    #[test]
    fn max_results_is_bounded() {
        assert_eq!(max_results_arg(&args(json!({"limit": 5000}))), 2000);
        assert_eq!(max_results_arg(&args(json!({"limit": -1}))), 300);
        assert_eq!(max_results_arg(&args(json!({"limit": 25}))), 25);
        assert_eq!(max_results_arg(&Args::new()), 300);
    }
}

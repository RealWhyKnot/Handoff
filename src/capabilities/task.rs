use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::proc::{fill, int_arg, load_specs, string_arg};
use super::pwsh::ps_single_quote;
use super::pwsh_out::run_pwsh_json;

const TEMPLATE: &str = r##"
$prefix = @@PREFIX@@
$state  = @@STATE@@
$max    = @@MAX@@

$tasks = Get-ScheduledTask -ErrorAction SilentlyContinue
if ($prefix) {
    $needle = $prefix.ToLowerInvariant()
    $tasks = $tasks | Where-Object { $_.TaskPath.ToLowerInvariant().StartsWith($needle) }
}
if ($state -ne 'all') {
    $tasks = $tasks | Where-Object { $_.State.ToString().ToLowerInvariant() -eq $state }
}

$results = $tasks |
    Sort-Object TaskPath, TaskName |
    Select-Object -First $max |
    ForEach-Object {
        $info = $null
        try { $info = $_ | Get-ScheduledTaskInfo -ErrorAction SilentlyContinue } catch { $info = $null }
        $triggers = @()
        foreach ($trig in @($_.Triggers)) {
            $triggers += [ordered]@{
                type       = if ($trig.CimClass) { [string]$trig.CimClass.CimClassName } else { 'Unknown' }
                enabled    = [bool]$trig.Enabled
                start      = if ($trig.StartBoundary) { [string]$trig.StartBoundary } else { $null }
            }
        }
        $actions = @()
        foreach ($act in @($_.Actions)) {
            $actions += [ordered]@{
                type     = if ($act.CimClass) { [string]$act.CimClass.CimClassName } else { 'Unknown' }
                execute  = $act.Execute
                arguments = $act.Arguments
            }
        }
        [ordered]@{
            name        = [string]$_.TaskName
            path        = [string]$_.TaskPath
            state       = [string]$_.State
            author      = [string]$_.Author
            description = [string]$_.Description
            last_run    = if ($info -and $info.LastRunTime) { $info.LastRunTime.ToUniversalTime().ToString("o") } else { $null }
            next_run    = if ($info -and $info.NextRunTime) { $info.NextRunTime.ToUniversalTime().ToString("o") } else { $null }
            last_result = if ($info) { [int]$info.LastTaskResult } else { $null }
            triggers    = $triggers
            actions     = $actions
        }
    }

[ordered]@{
    prefix  = $prefix
    state   = $state
    count   = @($results).Count
    max     = $max
    entries = @($results)
} | ConvertTo-Json -Compress -Depth 6
"##;

pub fn register(r: &mut Router) {
    for spec in load_specs(include_str!("specs/task.json")) {
        r.register_spec(spec, handler(task_list));
    }
}

pub fn is_valid_state(state: &str) -> bool {
    matches!(
        state,
        "all" | "ready" | "running" | "disabled" | "queued" | "unknown"
    )
}

pub fn list_script(args: &Args) -> Result<String, HandlerError> {
    let prefix = string_arg(args, "path_prefix", "").trim().to_string();
    let state = string_arg(args, "state", "").trim().to_lowercase();
    let mut max = int_arg(args, "limit", 0);

    if prefix.len() > 256 {
        return Err(HandlerError::plain("task.list: path_prefix too long"));
    }
    let state = if state.is_empty() {
        "all".to_string()
    } else {
        state
    };
    if !is_valid_state(&state) {
        return Err(HandlerError::plain(
            "task.list: state must be all, ready, running, disabled, queued, or unknown",
        ));
    }
    if max <= 0 {
        max = 300;
    }
    if max > 2000 {
        max = 2000;
    }

    Ok(fill(
        TEMPLATE,
        &[
            ("@@PREFIX@@", &ps_single_quote(&prefix)),
            ("@@STATE@@", &ps_single_quote(&state)),
            ("@@MAX@@", &max.to_string()),
        ],
    ))
}

async fn task_list(ctx: Ctx, args: Args) -> CapResult {
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
    fn registers_task_list() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["task.list"]);
        assert_eq!(r.spec_for("task.list").unwrap().params.len(), 3);
    }

    #[test]
    fn rejects_an_invalid_state() {
        let err = list_script(&args(json!({"state": "halfway"}))).unwrap_err();
        assert!(err.message.contains("state must be"), "{}", err.message);
    }

    #[test]
    fn rejects_an_over_long_prefix() {
        let err = list_script(&args(json!({"path_prefix": "a".repeat(300)}))).unwrap_err();
        assert!(err.message.contains("too long"), "{}", err.message);
    }

    #[test]
    fn normalizes_state_and_clamps_the_limit() {
        let script = list_script(&args(json!({
            "path_prefix": " \\Microsoft\\ ",
            "state": " Ready ",
            "limit": 99999
        })))
        .unwrap();
        assert!(script.contains("$prefix = '\\Microsoft\\'"));
        assert!(script.contains("$state  = 'ready'"));
        assert!(script.contains("$max    = 2000"));
        let script = list_script(&Args::new()).unwrap();
        assert!(script.contains("$state  = 'all'"));
        assert!(script.contains("$max    = 300"));
        assert!(!script.contains("@@"));
    }
}

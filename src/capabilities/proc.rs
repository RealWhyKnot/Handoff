use serde_json::Value;

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router, Spec};

use super::consent::require_risk_consent;
use super::pwsh::{json_script_handler, ps_single_quote, run_pwsh_json};

const LIST: &str = r##"
$procs = Get-CimInstance Win32_Process | Select-Object ProcessId,Name,ExecutablePath,CommandLine,@{n='WorkingSetMB';e={[math]::Round($_.WorkingSetSize/1MB,1)}},CreationDate
$procs | ConvertTo-Json -Compress -Depth 4
"##;

const SVC_LIST: &str =
    r##"Get-Service | Select-Object Name,DisplayName,Status,StartType | ConvertTo-Json -Compress"##;

const KILL_TEMPLATE: &str = r##"
$targetPid = @@PID@@
$proc = Get-Process -Id $targetPid -ErrorAction Stop
$name = $proc.ProcessName
$path = $proc.Path
Stop-Process -Id $targetPid -Force -ErrorAction Stop
[ordered]@{
    pid = $targetPid
    name = $name
    path = $path
    killed = $true
} | ConvertTo-Json -Compress
"##;

const FIND_TEMPLATE: &str = r##"
$q = [regex]::Escape([string]@@QUERY@@)
Get-CimInstance Win32_Process | Where-Object {
    $_.Name -match $q -or $_.ExecutablePath -match $q -or $_.CommandLine -match $q
} | Select-Object ProcessId,Name,ExecutablePath,CommandLine,ParentProcessId,SessionId,@{n='WorkingSetMB';e={[math]::Round($_.WorkingSetSize/1MB,1)}} |
    Select-Object -First @@MAX@@ |
    ConvertTo-Json -Compress -Depth 4
"##;

const CONTROL_TEMPLATE: &str = r##"
$name = @@NAME@@
$action = @@ACTION@@
$before = Get-Service -Name $name -ErrorAction Stop
switch ($action) {
    'start' { Start-Service -Name $name -ErrorAction Stop }
    'stop' { Stop-Service -Name $name -Force -ErrorAction Stop }
    'restart' { Restart-Service -Name $name -Force -ErrorAction Stop }
}
$after = Get-Service -Name $name -ErrorAction Stop
[ordered]@{
    name = $after.Name
    display_name = $after.DisplayName
    action = $action
    before = $before.Status.ToString()
    after = $after.Status.ToString()
} | ConvertTo-Json -Compress
"##;

const STATUS_TEMPLATE: &str = r##"
$name = @@NAME@@
$svc = Get-Service -Name $name -ErrorAction Stop
$cim = Get-CimInstance Win32_Service -Filter ("Name='" + $svc.Name.Replace("'","''") + "'") -ErrorAction SilentlyContinue
[ordered]@{
    name         = [string]$svc.Name
    display_name = [string]$svc.DisplayName
    status       = [string]$svc.Status
    start_type   = [string]$svc.StartType
    can_stop     = [bool]$svc.CanStop
    start_name   = if ($cim) { [string]$cim.StartName } else { $null }
    path         = if ($cim) { [string]$cim.PathName } else { $null }
    process_id   = if ($cim) { [int]$cim.ProcessId } else { $null }
    description  = if ($cim) { [string]$cim.Description } else { $null }
} | ConvertTo-Json -Compress
"##;

pub(super) fn load_specs(json: &str) -> Vec<Spec> {
    serde_json::from_str(json).expect("embedded capability specs are valid JSON")
}

pub(super) fn string_arg(args: &Args, name: &str, default: &str) -> String {
    match args.get(name) {
        Some(Value::String(s)) => s.clone(),
        _ => default.to_string(),
    }
}

pub(super) fn int_arg(args: &Args, name: &str, default: i64) -> i64 {
    args.get(name).and_then(Value::as_i64).unwrap_or(default)
}

pub(super) fn bool_arg(args: &Args, name: &str, default: bool) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(default)
}

pub(super) fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    'scan: while !rest.is_empty() {
        if rest.starts_with("@@") {
            for (key, value) in values {
                if rest.starts_with(key) {
                    out.push_str(value);
                    rest = &rest[key.len()..];
                    continue 'scan;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

pub(super) fn go_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c if c.is_control() => {
                if (c as u32) < 0x10000 {
                    out.push_str(&format!("\\u{:04x}", c as u32))
                } else {
                    out.push_str(&format!("\\U{:08x}", c as u32))
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn register(r: &mut Router) {
    for spec in load_specs(include_str!("specs/proc.json")) {
        let h = match spec.kind.as_str() {
            "proc.list" => json_script_handler(LIST),
            "svc.list" => json_script_handler(SVC_LIST),
            "proc.kill" => handler(proc_kill),
            "proc.find" => handler(proc_find),
            "svc.status" => handler(svc_status),
            "svc.control" => handler(svc_control),
            other => panic!("proc.json lists an unknown kind {other}"),
        };
        r.register_spec(spec, h);
    }
}

pub fn kill_pid_arg(args: &Args) -> Result<i64, HandlerError> {
    let pid = int_arg(args, "pid", 0);
    if pid <= 0 {
        return Err(HandlerError::plain(
            "proc.kill: 'pid' must be a positive integer",
        ));
    }
    if pid == i64::from(std::process::id()) {
        return Err(HandlerError::plain(
            "proc.kill: refusing to terminate the current handoff process",
        ));
    }
    Ok(pid)
}

async fn proc_kill(ctx: Ctx, args: Args) -> CapResult {
    let pid = kill_pid_arg(&args)?;
    require_risk_consent(
        &ctx.cancel,
        "proc.kill",
        &format!(
            "Terminates process ID {pid} on this computer. Unsaved work in that process can be lost."
        ),
    )
    .await?;
    let script = fill(KILL_TEMPLATE, &[("@@PID@@", &pid.to_string())]);
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

pub fn find_script(args: &Args) -> Result<String, HandlerError> {
    let query = string_arg(args, "query", "").trim().to_string();
    if query.is_empty() {
        return Err(HandlerError::plain("proc.find: 'query' is required"));
    }
    if query.len() > 128 {
        return Err(HandlerError::plain("proc.find: 'query' is too long"));
    }
    let mut max = int_arg(args, "limit", 0);
    if max <= 0 {
        max = 100;
    }
    if max > 500 {
        max = 500;
    }
    Ok(fill(
        FIND_TEMPLATE,
        &[
            ("@@QUERY@@", &ps_single_quote(&query)),
            ("@@MAX@@", &max.to_string()),
        ],
    ))
}

async fn proc_find(ctx: Ctx, args: Args) -> CapResult {
    let script = find_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

pub fn control_args(args: &Args) -> Result<(String, String), HandlerError> {
    let name = string_arg(args, "name", "").trim().to_string();
    let action = string_arg(args, "action", "").trim().to_lowercase();
    if name.is_empty() {
        return Err(HandlerError::plain("svc.control: 'name' is required"));
    }
    match action.as_str() {
        "start" | "stop" | "restart" => {}
        _ => {
            return Err(HandlerError::plain(
                "svc.control: action must be start, stop, or restart",
            ))
        }
    }
    Ok((name, action))
}

async fn svc_control(ctx: Ctx, args: Args) -> CapResult {
    let (name, action) = control_args(&args)?;
    require_risk_consent(
        &ctx.cancel,
        "svc.control",
        &format!(
            "Runs '{}' against Windows service {}. This can interrupt software or device functionality.",
            action,
            go_quote(&name)
        ),
    )
    .await?;
    let script = fill(
        CONTROL_TEMPLATE,
        &[
            ("@@NAME@@", &ps_single_quote(&name)),
            ("@@ACTION@@", &ps_single_quote(&action)),
        ],
    );
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

pub fn status_script(args: &Args) -> Result<String, HandlerError> {
    let name = string_arg(args, "name", "").trim().to_string();
    if name.is_empty() {
        return Err(HandlerError::plain("svc.status: 'name' is required"));
    }
    Ok(fill(
        STATUS_TEMPLATE,
        &[("@@NAME@@", &ps_single_quote(&name))],
    ))
}

async fn svc_status(ctx: Ctx, args: Args) -> CapResult {
    let script = status_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

#[cfg(test)]
mod tests {
    use super::super::consent::testing::SessionGuard;
    use super::*;
    use crate::consent::Prompt;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn counting_prompt(allow: bool) -> (Prompt, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let prompt: Prompt = Arc::new(move |_, _| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(allow)
            })
        });
        (prompt, calls)
    }

    #[test]
    fn registers_the_proc_and_service_kinds() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(
            r.kinds(),
            vec![
                "proc.find",
                "proc.kill",
                "proc.list",
                "svc.control",
                "svc.list",
                "svc.status"
            ]
        );
        assert!(r.spec_for("proc.kill").unwrap().risky);
        assert!(r.spec_for("svc.control").unwrap().risky);
        assert!(!r.spec_for("svc.status").unwrap().risky);
    }

    #[test]
    fn fill_substitutes_in_one_pass() {
        let out = fill(
            "a @@X@@ b @@Y@@ @@Z@@",
            &[("@@X@@", "@@Y@@"), ("@@Y@@", "2")],
        );
        assert_eq!(out, "a @@Y@@ b 2 @@Z@@");
    }

    #[test]
    fn go_quote_matches_go_percent_q() {
        assert_eq!(go_quote("Spooler"), "\"Spooler\"");
        assert_eq!(go_quote("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(go_quote("\u{1}"), "\"\\x01\"");
        assert_eq!(go_quote("caf\u{e9}"), "\"caf\u{e9}\"");
    }

    #[test]
    fn proc_find_rejects_empty_and_over_long_queries() {
        let err = find_script(&args(json!({"query": "  "}))).unwrap_err();
        assert!(
            err.message.contains("'query' is required"),
            "{}",
            err.message
        );
        let err = find_script(&args(json!({"query": "x".repeat(130)}))).unwrap_err();
        assert!(err.message.contains("too long"), "{}", err.message);
    }

    #[test]
    fn proc_find_quotes_the_query_and_clamps_the_limit() {
        let script = find_script(&args(json!({"query": " O'Neil ", "limit": 9999}))).unwrap();
        assert!(script.contains("[string]'O''Neil'"));
        assert!(script.contains("-First 500"));
        let script = find_script(&args(json!({"query": "x", "limit": -3}))).unwrap();
        assert!(script.contains("-First 100"));
        assert!(!script.contains("@@"));
    }

    #[test]
    fn svc_status_requires_a_name() {
        let err = status_script(&args(json!({"name": " "}))).unwrap_err();
        assert!(
            err.message.contains("'name' is required"),
            "{}",
            err.message
        );
        let script = status_script(&args(json!({"name": "Spooler"}))).unwrap();
        assert!(script.contains("$name = 'Spooler'"));
    }

    #[test]
    fn svc_control_validates_its_arguments() {
        let err = control_args(&args(json!({"action": "start"}))).unwrap_err();
        assert!(
            err.message.contains("'name' is required"),
            "{}",
            err.message
        );
        let err = control_args(&args(json!({"name": "x", "action": "kill"}))).unwrap_err();
        assert_eq!(
            err.message,
            "svc.control: action must be start, stop, or restart"
        );
        let (name, action) =
            control_args(&args(json!({"name": " x ", "action": " STOP "}))).unwrap();
        assert_eq!((name.as_str(), action.as_str()), ("x", "stop"));
    }

    #[test]
    fn kill_pid_must_be_positive_and_not_ours() {
        let err = kill_pid_arg(&args(json!({"pid": 0}))).unwrap_err();
        assert!(err.message.contains("positive integer"), "{}", err.message);
        let err = kill_pid_arg(&Args::new()).unwrap_err();
        assert!(err.message.contains("positive integer"), "{}", err.message);
        let err = kill_pid_arg(&args(json!({"pid": std::process::id()}))).unwrap_err();
        assert!(
            err.message.contains("current handoff process"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn proc_kill_refuses_the_current_process_before_consent() {
        let (prompt, calls) = counting_prompt(true);
        let _guard = SessionGuard::with_prompt(prompt);
        let mut r = Router::new();
        register(&mut r);
        let out = r
            .dispatch(
                Ctx::default(),
                "proc.kill",
                args(json!({"pid": std::process::id()})),
            )
            .await;
        assert!(!out.ok);
        assert!(
            out.error.contains("current handoff process"),
            "{}",
            out.error
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn proc_kill_and_svc_control_stop_at_a_declined_prompt() {
        let (prompt, calls) = counting_prompt(false);
        let _guard = SessionGuard::with_prompt(prompt);
        let mut r = Router::new();
        register(&mut r);
        let out = r
            .dispatch(Ctx::default(), "proc.kill", args(json!({"pid": 4_000_000})))
            .await;
        assert!(!out.ok);
        assert_eq!(out.error, "the host declined this request");
        let out = r
            .dispatch(
                Ctx::default(),
                "svc.control",
                args(json!({"name": "NoSuchService", "action": "stop"})),
            )
            .await;
        assert!(!out.ok);
        assert_eq!(out.error, "the host declined this request");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

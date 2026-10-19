use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::consent::require_risk_consent;
use super::proc::{load_specs, string_arg};
use super::pwsh::run_pwsh_full;

const SCRIPT_CAP: usize = 64 * 1024;
const LIMIT: usize = 10;
const WINDOW: Duration = Duration::from_secs(60);

static HISTORY: LazyLock<Mutex<Vec<Instant>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub fn register(r: &mut Router) {
    for spec in load_specs(include_str!("specs/ps.json")) {
        r.register_spec(spec, handler(ps_exec));
    }
}

fn take_slot(now: Instant) -> Result<(), HandlerError> {
    let mut history = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    history.retain(|t| now.saturating_duration_since(*t) < WINDOW);
    if history.len() >= LIMIT {
        return Err(HandlerError::plain(format!(
            "ps.exec: rate limit reached ({LIMIT} in last 1m0s)"
        )));
    }
    history.push(now);
    Ok(())
}

pub fn exec_script(args: &Args) -> Result<String, HandlerError> {
    let script = string_arg(args, "script", "").trim().to_string();
    if script.is_empty() {
        return Err(HandlerError::plain("ps.exec: 'script' is required"));
    }
    if script.len() > SCRIPT_CAP {
        return Err(HandlerError::plain(format!(
            "ps.exec: script is {} bytes; cap is {}",
            script.len(),
            SCRIPT_CAP
        )));
    }
    Ok(script)
}

async fn ps_exec(ctx: Ctx, args: Args) -> CapResult {
    let script = exec_script(&args)?;
    require_risk_consent(
        &ctx.cancel,
        "ps.exec",
        "Runs arbitrary PowerShell code on this computer. The script can read, change, or delete files and can start programs as the current user.",
    )
    .await?;
    take_slot(Instant::now())?;
    match run_pwsh_full(&ctx.cancel, &script).await {
        Ok(res) => Ok(json!({
            "exit_code": res.exit_code,
            "stdout": String::from_utf8_lossy(&res.stdout),
            "stderr": String::from_utf8_lossy(&res.stderr),
        })),
        Err((_, failure)) => Err(failure.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::consent::testing::SessionGuard;
    use super::*;
    use crate::consent::Prompt;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn reset_history() {
        HISTORY.lock().unwrap_or_else(|e| e.into_inner()).clear();
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
    fn registers_ps_exec_as_risky() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["ps.exec"]);
        let spec = r.spec_for("ps.exec").unwrap();
        assert!(spec.risky);
        assert_eq!(spec.params[0].max_bytes, SCRIPT_CAP);
    }

    #[test]
    fn script_is_required_and_capped() {
        let err = exec_script(&args(json!({"script": "   "}))).unwrap_err();
        assert!(
            err.message.contains("'script' is required"),
            "{}",
            err.message
        );
        let err = exec_script(&args(json!({"script": "x".repeat(SCRIPT_CAP + 1)}))).unwrap_err();
        assert!(err.message.contains("cap is 65536"), "{}", err.message);
        assert_eq!(
            exec_script(&args(json!({"script": "  hi  "}))).unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn oversized_script_is_rejected_before_consent() {
        let (prompt, calls) = counting_prompt(true);
        let _guard = SessionGuard::with_prompt(prompt);
        reset_history();
        let out = ps_exec(
            Ctx::default(),
            args(json!({"script": "x".repeat(SCRIPT_CAP + 1)})),
        )
        .await
        .unwrap_err();
        assert!(out.message.contains("cap is 65536"), "{}", out.message);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn declined_consent_stops_before_running_anything() {
        let (prompt, calls) = counting_prompt(false);
        let _guard = SessionGuard::with_prompt(prompt);
        reset_history();
        let err = ps_exec(Ctx::default(), args(json!({"script": "Write-Output ok"})))
            .await
            .unwrap_err();
        assert_eq!(err.message, "the host declined this request");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(HISTORY.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    #[test]
    fn the_eleventh_run_in_a_minute_is_refused() {
        let _guard = SessionGuard::new();
        reset_history();
        let now = Instant::now();
        for _ in 0..LIMIT {
            take_slot(now).unwrap();
        }
        let err = take_slot(now).unwrap_err();
        assert_eq!(err.message, "ps.exec: rate limit reached (10 in last 1m0s)");
        reset_history();
    }

    #[tokio::test]
    async fn ps_exec_uses_session_consent_and_reports_streams() {
        if !cfg!(windows) {
            return;
        }
        let (prompt, _calls) = counting_prompt(true);
        let _guard = SessionGuard::with_prompt(prompt);
        reset_history();
        let res = ps_exec(Ctx::default(), args(json!({"script": "Write-Output ok"})))
            .await
            .unwrap();
        assert_eq!(res["exit_code"], json!(0));
        assert!(res["stdout"].as_str().unwrap().contains("ok"));
        assert_eq!(res["stderr"], json!(""));
    }

    #[tokio::test]
    async fn a_failing_script_is_an_error_with_its_exit_code() {
        if !cfg!(windows) {
            return;
        }
        let (prompt, _calls) = counting_prompt(true);
        let _guard = SessionGuard::with_prompt(prompt);
        reset_history();
        let mut r = Router::new();
        register(&mut r);
        let out = r
            .dispatch(Ctx::default(), "ps.exec", args(json!({"script": "exit 3"})))
            .await;
        assert!(!out.ok);
        assert!(
            out.error.starts_with("powershell exited 3"),
            "{}",
            out.error
        );
        assert_eq!(out.detail.unwrap()["exit_code"], json!(3));
    }
}

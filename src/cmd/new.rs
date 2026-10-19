use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use futures_util::FutureExt;
use tokio_util::sync::CancellationToken;

use super::flags::{parse_options, Def, FlagError, Options};
use super::keys::start_keyboard_control;
use super::summary::{summarize_args, summarize_result};
use crate::audit::{self, Logger};
use crate::consent::{category_for, Decision};
use crate::dispatch::{Ctx, Outcome, Router};
use crate::relay::{self, Bridge, Command, RecvError};
use crate::{capabilities, slog, stayawake, supportlog, util, VERSION};

const NEW_FLAGS: [Def; 2] = [
    Def::string(
        "consent",
        "ask or deny; deny runs a strictly read-only session",
        "ask",
    ),
    Def::boolean(
        "no-keep-awake",
        "let this computer sleep during the session",
    ),
];

const MINT_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub fn new(args: &[String]) -> i32 {
    let (opts, parsed) = match parse_options("new", args, &NEW_FLAGS) {
        Ok(v) => v,
        Err(FlagError::Help) => return 2,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    let consent_mode = parsed.string("consent");
    let no_keep_awake = parsed.boolean("no-keep-awake");
    match consent_mode.as_str() {
        "ask" => {}
        "deny" => capabilities::deny_all_risky(),
        _ => {
            eprintln!("--consent must be ask or deny");
            return 2;
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    let code = runtime.block_on(session(opts, consent_mode == "deny", no_keep_awake));
    runtime.shutdown_timeout(Duration::from_secs(2));
    code
}

pub fn short_sid(token: &str) -> String {
    token.get(..11).unwrap_or(token).to_string()
}

async fn session(opts: Options, read_only: bool, no_keep_awake: bool) -> i32 {
    let relay_base = opts.relay.clone();
    slog!("session start relay={} version={}", relay_base, VERSION);

    let root = CancellationToken::new();

    let log = match Logger::new_in_dir(&opts.audit_dir) {
        Ok(l) => Some(Arc::new(l)),
        Err(err) => {
            slog!("audit log unavailable: {err}");
            eprintln!("warning: audit log unavailable: {err}");
            None
        }
    };

    let mint = match tokio::time::timeout(MINT_TIMEOUT, relay::mint(&relay_base)).await {
        Ok(Ok(m)) => m,
        Ok(Err(err)) => return mint_failed(&err),
        Err(_) => return mint_failed("context deadline exceeded"),
    };
    let sid = short_sid(&mint.view_token);
    slog!("mint ok sid={sid}");

    let log_path = supportlog::path()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    println!("relay:  {relay_base}");
    println!("log:    {log_path}");
    if let Some(log) = &log {
        println!("audit:  {}", log.dir().display());
    }
    let keep_awake = !no_keep_awake && opts.keep_awake;
    if keep_awake {
        println!("keeping this PC awake while the session runs (--no-keep-awake to turn off)");
    }
    if read_only {
        println!("read-only session: every request that would change this PC is refused");
    }
    println!();
    println!("Session started. Send this link to the person helping you:");
    println!("   {}", mint.view_url);
    println!();
    println!("press q then Enter to end the session, or ? for other keys.");
    println!();

    {
        let root = root.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                slog!("shutdown requested by signal");
                println!("\nshutting down...");
                root.cancel();
            }
        });
    }

    if keep_awake {
        stayawake::start(root.clone());
    }
    start_keyboard_control(root.clone(), log.clone());

    let dial_cancel = root.child_token();
    let dialed = tokio::time::timeout(
        DIAL_TIMEOUT,
        Bridge::dial(&relay_base, &mint.write_token, &dial_cancel),
    )
    .await;
    let bridge = match dialed {
        Ok(Ok(b)) => b,
        Ok(Err(err)) => return dial_failed(&sid, &err),
        Err(_) => {
            dial_cancel.cancel();
            return dial_failed(&sid, "context deadline exceeded");
        }
    };
    slog!("bridge connected sid={sid}");

    let mut router = Router::new();
    capabilities::register_all(&mut router, Some(bridge.clone()));
    let router = Arc::new(router);
    let kinds = router.kinds();
    slog!("capabilities registered count={}", kinds.len());
    println!("ready: {} commands available\n", kinds.len());

    let hostname = util::hostname();
    if let Err(err) = bridge
        .send_hello(&hostname, VERSION, &kinds, &router.specs())
        .await
    {
        slog!("hello failed sid={sid}: {err}");
        eprintln!("hello failed: {err}");
        bridge.close().await;
        return 1;
    }
    slog!("hello sent sid={sid} hostname={hostname}");
    bridge.start_heartbeat(root.clone());

    let jobs = Jobs::new(
        root.clone(),
        sid.clone(),
        router,
        bridge.clone(),
        log.clone(),
    );

    let mut backoff = ReconnectBackoff::new();
    while recv_iteration(&root, &sid, &bridge, &mut backoff, &jobs).await {}

    jobs.cancel_all();
    root.cancel();
    bridge.close().await;
    if let Some(log) = &log {
        log.close();
    }
    0
}

fn mint_failed(err: &str) -> i32 {
    slog!("mint failed: {err}");
    eprintln!("could not mint session: {err}");
    1
}

fn dial_failed(sid: &str, err: &str) -> i32 {
    slog!("bridge dial failed sid={sid}: {err}");
    eprintln!("could not open bridge: {err}");
    1
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "unknown panic".to_string()
}

pub async fn recv_iteration(
    cancel: &CancellationToken,
    sid: &str,
    bridge: &Bridge,
    backoff: &mut ReconnectBackoff,
    jobs: &Jobs,
) -> bool {
    let step = AssertUnwindSafe(recv_step(cancel, sid, bridge, backoff, jobs)).catch_unwind();
    match step.await {
        Ok(keep_going) => keep_going,
        Err(payload) => {
            slog!(
                "recv loop panic sid={sid}: {}",
                panic_text(payload.as_ref())
            );
            true
        }
    }
}

async fn recv_step(
    cancel: &CancellationToken,
    sid: &str,
    bridge: &Bridge,
    backoff: &mut ReconnectBackoff,
    jobs: &Jobs,
) -> bool {
    let cmd = match bridge.recv(cancel).await {
        Ok(cmd) => cmd,
        Err(err) => {
            if matches!(err, RecvError::Cancelled) || cancel.is_cancelled() {
                slog!("recv ended sid={sid} err={err}");
                return false;
            }
            slog!("recv ended sid={sid} err={err}; reconnecting");
            eprintln!("connection lost; reconnecting...");
            return reconnect_bridge(cancel, sid, bridge, backoff).await;
        }
    };
    backoff.reset();
    slog!("command received sid={sid} id={} kind={}", cmd.id, cmd.kind);

    if cmd.kind == "control.cancel" {
        let target = cmd
            .extras
            .get("target_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if target.is_empty() {
            slog!("cancel control missing target sid={sid} id={}", cmd.id);
            return true;
        }
        if jobs.cancel(&target) {
            println!("[cancel] {target}");
            slog!("command cancel requested sid={sid} id={target}");
        } else {
            slog!("cancel target not active sid={sid} id={target}");
        }
        return true;
    }
    if capabilities::is_tunnel_frame_kind(&cmd.kind) {
        capabilities::enqueue_tunnel_frame(&cmd.kind, &cmd.extras);
        return true;
    }

    let summary = summarize_args(&cmd.extras);
    if summary.is_empty() {
        println!("[cmd] {}  {}", cmd.id, cmd.kind);
    } else {
        println!("[cmd] {}  {}  {}", cmd.id, cmd.kind, summary);
    }
    jobs.start(cmd);
    true
}

#[derive(Debug, Clone)]
pub struct ReconnectBackoff {
    next: Duration,
}

impl ReconnectBackoff {
    const FIRST: Duration = Duration::from_millis(500);
    const CAP: Duration = Duration::from_secs(5);

    pub fn new() -> ReconnectBackoff {
        ReconnectBackoff { next: Self::FIRST }
    }

    pub fn next_delay(&mut self) -> Duration {
        if self.next.is_zero() {
            self.next = Self::FIRST;
        }
        let d = self.next;
        self.next = (self.next * 2).min(Self::CAP);
        d
    }

    pub fn reset(&mut self) {
        self.next = Self::FIRST;
    }
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        ReconnectBackoff::new()
    }
}

pub async fn sleep_cancel(cancel: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => true,
        _ = cancel.cancelled() => false,
    }
}

async fn reconnect_bridge(
    cancel: &CancellationToken,
    sid: &str,
    bridge: &Bridge,
    backoff: &mut ReconnectBackoff,
) -> bool {
    loop {
        let delay = backoff.next_delay();
        if !sleep_cancel(cancel, delay).await {
            slog!("reconnect cancelled sid={sid}");
            return false;
        }

        let attempt = cancel.child_token();
        let result = match tokio::time::timeout(RECONNECT_TIMEOUT, bridge.reconnect(&attempt)).await
        {
            Ok(r) => r,
            Err(_) => {
                attempt.cancel();
                Err("context deadline exceeded".to_string())
            }
        };
        match result {
            Ok(()) => {
                backoff.reset();
                slog!("bridge reconnected sid={sid}");
                println!("reconnected.");
                return true;
            }
            Err(err) => {
                if cancel.is_cancelled() {
                    slog!("reconnect ended sid={sid} err=context canceled");
                    return false;
                }
                slog!(
                    "reconnect failed sid={sid} delay={} err={err}",
                    util::go_duration_string(delay)
                );
                eprintln!("reconnect failed: {err}");
            }
        }
    }
}

#[derive(Clone)]
pub struct Jobs {
    root: CancellationToken,
    sid: String,
    router: Arc<Router>,
    bridge: Bridge,
    audit: Option<Arc<Logger>>,
    active: Arc<Mutex<HashMap<String, CancellationToken>>>,
    audit_fail_once: Arc<Once>,
}

struct ActiveGuard {
    active: Arc<Mutex<HashMap<String, CancellationToken>>>,
    id: String,
    token: CancellationToken,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
        self.token.cancel();
    }
}

impl Jobs {
    pub fn new(
        root: CancellationToken,
        sid: String,
        router: Arc<Router>,
        bridge: Bridge,
        audit: Option<Arc<Logger>>,
    ) -> Jobs {
        Jobs {
            root,
            sid,
            router,
            bridge,
            audit,
            active: Arc::new(Mutex::new(HashMap::new())),
            audit_fail_once: Arc::new(Once::new()),
        }
    }

    fn active(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn start(&self, cmd: Command) {
        let token = self.root.child_token();
        self.active().insert(cmd.id.clone(), token.clone());

        let jobs = self.clone();
        tokio::spawn(async move {
            let _guard = ActiveGuard {
                active: jobs.active.clone(),
                id: cmd.id.clone(),
                token: token.clone(),
            };

            let timed_out = Arc::new(AtomicBool::new(false));
            let timer = (cmd.timeout_ms > 0).then(|| {
                let token = token.clone();
                let timed_out = timed_out.clone();
                let after = Duration::from_millis(cmd.timeout_ms as u64);
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    timed_out.store(true, Ordering::SeqCst);
                    token.cancel();
                })
            });

            let mut out = jobs
                .router
                .dispatch(Ctx::new(token.clone()), &cmd.kind, cmd.extras.clone())
                .await;
            if let Some(timer) = timer {
                timer.abort();
            }
            if token.is_cancelled() {
                out.ok = false;
                out.error = if timed_out.load(Ordering::SeqCst) {
                    "command timed out".to_string()
                } else {
                    "command cancelled".to_string()
                };
            }

            jobs.write_audit(&cmd, &out);
            if let Err(err) = jobs.bridge.send_command_result(&cmd.id, &out).await {
                slog!("send result failed sid={} id={}: {err}", jobs.sid, cmd.id);
                eprintln!("could not send result: {err}");
                return;
            }
            slog!(
                "command result sent sid={} id={} ok={} elapsed_ms={}",
                jobs.sid,
                cmd.id,
                out.ok,
                out.elapsed_ms
            );
            let status = if out.ok { "ok" } else { "failed" };
            let detail = summarize_result(&out);
            if detail.is_empty() {
                println!("       -> {status}  {}ms", out.elapsed_ms);
            } else {
                println!("       -> {status}  {}ms  {detail}", out.elapsed_ms);
            }
        });
    }

    pub fn cancel(&self, id: &str) -> bool {
        let token = self.active().get(id).cloned();
        match token {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    pub fn cancel_all(&self) {
        let tokens: Vec<CancellationToken> = self.active().values().cloned().collect();
        for token in tokens {
            token.cancel();
        }
    }

    pub fn active_count(&self) -> usize {
        self.active().len()
    }

    fn write_audit(&self, cmd: &Command, out: &Outcome) {
        let Some(log) = &self.audit else {
            return;
        };
        let decision = capabilities::last_consent_decision(&cmd.kind);
        let result = if decision == Decision::PromptDeny {
            "denied"
        } else if out.ok {
            "ok"
        } else {
            "err"
        };
        let entry = audit::Entry {
            sid: self.sid.clone(),
            cap: cmd.kind.clone(),
            args: audit::trim_args(&cmd.extras),
            consent: decision.as_str().to_string(),
            consent_scope: category_for(&cmd.kind)
                .map(|c| c.as_str().to_string())
                .unwrap_or_default(),
            result: result.to_string(),
            elapsed_ms: out.elapsed_ms,
            detail: out.error.clone(),
            ..audit::Entry::default()
        };
        if let Err(err) = log.write(entry) {
            self.audit_fail_once.call_once(|| {
                slog!("audit write failed sid={}: {err}", self.sid);
                eprintln!("warning: audit log is not recording: {err}");
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_five_seconds_and_resets() {
        let mut b = ReconnectBackoff::new();
        let steps: Vec<u128> = (0..6).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(steps, vec![500, 1000, 2000, 4000, 5000, 5000]);
        b.reset();
        assert_eq!(b.next_delay().as_millis(), 500);
    }

    #[test]
    fn short_sid_keeps_the_first_eleven_characters() {
        assert_eq!(short_sid("n1_AbCdEfGhIjKl"), "n1_AbCdEfGh");
        assert_eq!(short_sid("short"), "short");
    }

    #[test]
    fn bad_consent_mode_is_a_usage_error() {
        let args: Vec<String> = ["--relay", "http://127.0.0.1:9", "--consent", "maybe"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(new(&args), 2);
    }

    #[test]
    fn unknown_flag_is_a_usage_error() {
        let args: Vec<String> = ["--bogus"].iter().map(|s| s.to_string()).collect();
        assert_eq!(new(&args), 2);
    }

    #[test]
    fn help_exits_with_two() {
        let args: Vec<String> = ["-h"].iter().map(|s| s.to_string()).collect();
        assert_eq!(new(&args), 2);
    }

    #[tokio::test]
    async fn sleep_cancel_returns_false_when_cancelled() {
        let token = CancellationToken::new();
        token.cancel();
        assert!(!sleep_cancel(&token, Duration::from_secs(30)).await);
        let fresh = CancellationToken::new();
        assert!(sleep_cancel(&fresh, Duration::from_millis(5)).await);
    }
}

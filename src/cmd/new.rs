use std::collections::HashMap;
use std::io::IsTerminal;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::flags::{parse_options, Def, FlagError, Options};
use super::host::{self, tag, Board, Ending};
use super::keys::{start_keyboard_control, ENDING};
use super::summary::{describe_args, summarize_result};
use crate::audit::{self, Logger};
use crate::consent::{self, category_for, Decision};
use crate::console::{self, human_duration, paint, plain_error, Tone};
use crate::dispatch::{Ctx, Outcome, Progress, Router};
use crate::relay::{self, Bridge, Command, Delivery, RecvError};
use crate::{capabilities, slog, stayawake, supportlog, util, VERSION};

const NEW_FLAGS: [Def; 2] = [
    Def::string(
        "consent",
        "ask: check with you before any change. deny: allow no changes at all",
        "ask",
    ),
    Def::boolean(
        "no-keep-awake",
        "let this computer go to sleep while the session is open",
    ),
];

const MINT_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);
const STILL_TRYING_AFTER: usize = 3;

pub const ENDED: &str = "Session ended. The helper can't reach this computer any more.";
pub const LOST: &str = "The connection to the Handoff server dropped. Handoff is reconnecting, so you don't need to do anything yet.";
pub const STILL_TRYING: &str = "Still trying to reconnect. Check that this computer is online. The helper's link will work again once Handoff is back.";
pub const RECONNECTED: &str = "Reconnected. The helper can carry on with the same link.";

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
            eprintln!("--consent can only be ask or deny");
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

fn print_welcome(board: &Board, read_only: bool, keep_awake: bool, copied: bool) {
    println!();
    println!(
        "{}",
        paint(Tone::Bold, "Send this link to the person helping you:")
    );
    println!("   {}", paint(Tone::Cyan, board.link()));
    if copied {
        println!("The link is copied, so you can paste it straight into a message.");
    }
    println!();
    if read_only {
        println!("This session can only look. The helper can't change anything on this computer.");
    } else {
        println!("If the helper wants to change something, Handoff will ask you first.");
    }
    if keep_awake {
        println!("This computer will stay awake until the session ends.");
    }
    println!("Type q and press Enter to end the session, or ? for other options.");
    println!();
}

async fn session(opts: Options, read_only: bool, no_keep_awake: bool) -> i32 {
    console::init();
    let relay_base = opts.relay.clone();
    slog!("session start relay={} version={}", relay_base, VERSION);

    let root = CancellationToken::new();

    let log = match Logger::new_in_dir(&opts.audit_dir) {
        Ok(l) => Some(Arc::new(l)),
        Err(err) => {
            slog!("audit log unavailable: {err}");
            console::eline(&format!(
                "Handoff can't keep a record of this session: {}",
                plain_error(&err.to_string())
            ));
            None
        }
    };

    let mint = match tokio::time::timeout(MINT_TIMEOUT, relay::mint(&relay_base)).await {
        Ok(Ok(m)) => m,
        Ok(Err(err)) => return mint_failed(&relay_base, &err),
        Err(_) => return mint_failed(&relay_base, "it took too long to answer"),
    };
    let sid = short_sid(&mint.view_token);
    slog!("mint ok sid={sid}");

    let board = Arc::new(Board::new(&mint.view_url, read_only));
    if opts.verbose {
        let log_path = supportlog::path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        println!("Server:          {relay_base}");
        println!("Support log:     {log_path}");
        if let Some(log) = &log {
            println!("Activity record: {}", log.dir().display());
        }
    }
    let keep_awake = !no_keep_awake && opts.keep_awake;
    let copied = std::io::stdout().is_terminal() && host::copy_to_clipboard(board.link());
    slog!("link copied={copied}");
    print_welcome(&board, read_only, keep_awake, copied);

    {
        let root = root.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                slog!("shutdown requested by signal");
                println!();
                console::line(ENDING);
                root.cancel();
            }
        });
    }

    if keep_awake {
        stayawake::start(root.clone());
    }
    start_keyboard_control(root.clone(), log.clone(), board.clone());

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
            return dial_failed(&sid, "it took too long to answer");
        }
    };
    slog!("bridge connected sid={sid}");

    let mut router = Router::new();
    capabilities::register_all(&mut router, Some(bridge.clone()));
    let router = Arc::new(router);
    let kinds = router.kinds();
    let specs = router.specs();
    board.learn_labels(&specs);
    board.watch_consent();
    slog!("capabilities registered count={}", kinds.len());

    let hostname = util::hostname();
    bridge.want_presence();
    if let Err(err) = bridge.send_hello(&hostname, VERSION, &kinds, &specs).await {
        slog!("hello failed sid={sid}: {err}");
        console::eline(&format!(
            "Couldn't finish connecting to the Handoff server: {}. Try again in a moment.",
            plain_error(&err)
        ));
        consent::set_observer(None);
        bridge.close().await;
        return 1;
    }
    slog!("hello sent sid={sid} hostname={hostname}");
    bridge.start_heartbeat(root.clone());
    console::line("Connected. Waiting for the helper to open the link.");
    if opts.verbose {
        console::line(&format!("{} commands are available.", kinds.len()));
    }

    let jobs = Jobs::new(
        root.clone(),
        sid.clone(),
        router,
        bridge.clone(),
        log.clone(),
        board.clone(),
    );

    let mut backoff = ReconnectBackoff::new();
    while recv_iteration(&root, &sid, &bridge, &mut backoff, &jobs).await {}

    jobs.cancel_all();
    let drain_until = tokio::time::Instant::now() + SHUTDOWN_DRAIN;
    while jobs.active_count() > 0 && tokio::time::Instant::now() < drain_until {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    root.cancel();
    bridge.close().await;
    consent::set_observer(None);
    if let Some(log) = &log {
        log.close();
    }
    console::line(&paint(Tone::Bold, ENDED));
    console::line(&host::exit_summary(board.stats(), board.elapsed()));
    0
}

fn mint_failed(relay_base: &str, err: &str) -> i32 {
    slog!("mint failed: {err}");
    console::eline(&format!(
        "Couldn't start a session. Handoff couldn't reach the server at {relay_base}: {}. Check that this computer is online, then try again.",
        plain_error(err)
    ));
    1
}

fn dial_failed(sid: &str, err: &str) -> i32 {
    slog!("bridge dial failed sid={sid}: {err}");
    console::eline(&format!(
        "Couldn't connect to the Handoff server: {}. Check that this computer is online, then try again.",
        plain_error(err)
    ));
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
            console::line(&paint(Tone::Yellow, LOST));
            return reconnect_bridge(cancel, sid, bridge, backoff).await;
        }
    };
    backoff.reset();
    slog!("command received sid={sid} id={} kind={}", cmd.id, cmd.kind);

    if cmd.kind == relay::PRESENCE_KIND {
        let viewers = cmd
            .extras
            .get("viewers")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        slog!("presence sid={sid} viewers={viewers}");
        let generation = jobs.board.see_viewers(viewers);
        let board = jobs.board.clone();
        tokio::spawn(async move {
            tokio::time::sleep(host::PRESENCE_SETTLE).await;
            if let Some(text) = board.settle_viewers(generation) {
                console::line(&text);
            }
        });
        return true;
    }
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
            match jobs.board.number_of(&target) {
                Some(num) => {
                    console::line(&format!("{}  The helper asked to stop this.", tag(num)))
                }
                None => console::line("The helper asked to stop a command."),
            }
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

    let view = describe_args(&cmd.extras);
    let num = jobs.board.begin(&cmd.id, &cmd.kind);
    slog!("command number sid={sid} id={} num={num}", cmd.id);
    let label = jobs.board.label(&cmd.kind);
    console::lines(&host::command_lines(
        num,
        label.as_deref(),
        &cmd.kind,
        &view,
        console::body_width(),
    ));
    jobs.start(cmd, num);
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
    let mut failures = 0usize;
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
                Err("it took too long to answer".to_string())
            }
        };
        match result {
            Ok(()) => {
                backoff.reset();
                slog!("bridge reconnected sid={sid}");
                console::line(&paint(Tone::Green, RECONNECTED));
                return true;
            }
            Err(err) => {
                if cancel.is_cancelled() {
                    slog!("reconnect ended sid={sid} err=stopped");
                    return false;
                }
                slog!(
                    "reconnect failed sid={sid} delay={} err={err}",
                    util::go_duration_string(delay)
                );
                failures += 1;
                if failures == STILL_TRYING_AFTER {
                    console::line(&paint(Tone::Yellow, STILL_TRYING));
                }
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
    board: Arc<Board>,
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

const PROGRESS_FLUSH: Duration = Duration::from_millis(250);
const PROGRESS_DRAIN: Duration = Duration::from_secs(2);
const PROGRESS_FRAME_CAP: usize = 16 * 1024;
const PROGRESS_TOTAL_CAP: usize = 1024 * 1024;
const PROGRESS_STOPPED: &str =
    "\nLive output stopped at 1 MB. The full output arrives with the result.\n";

fn progress_forwarder(bridge: Bridge, id: String) -> (Progress, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let progress: Progress = Arc::new(move |text| {
        let _ = tx.send(text);
    });
    let task = tokio::spawn(async move {
        let mut sent = 0usize;
        while let Some(first) = rx.recv().await {
            let mut batch = first;
            let deadline = tokio::time::Instant::now() + PROGRESS_FLUSH;
            while batch.len() < PROGRESS_FRAME_CAP {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(more)) => batch.push_str(&more),
                    _ => break,
                }
            }
            if sent >= PROGRESS_TOTAL_CAP {
                continue;
            }
            let room = PROGRESS_TOTAL_CAP - sent;
            let over = batch.len() > room;
            truncate_at_char(&mut batch, room);
            sent += batch.len();
            if over {
                batch.push_str(PROGRESS_STOPPED);
                sent = PROGRESS_TOTAL_CAP;
            }
            for piece in split_at_chars(&batch, PROGRESS_FRAME_CAP) {
                if let Err(err) = bridge.send_command_progress(&id, piece).await {
                    slog!("send progress failed id={id}: {err}");
                    break;
                }
            }
        }
    });
    (progress, task)
}

fn truncate_at_char(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

fn split_at_chars(s: &str, max: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = s;
    while rest.len() > max {
        let mut end = max;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (head, tail) = rest.split_at(end);
        pieces.push(head);
        rest = tail;
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

impl Jobs {
    pub fn new(
        root: CancellationToken,
        sid: String,
        router: Arc<Router>,
        bridge: Bridge,
        audit: Option<Arc<Logger>>,
        board: Arc<Board>,
    ) -> Jobs {
        Jobs {
            root,
            sid,
            router,
            bridge,
            audit,
            active: Arc::new(Mutex::new(HashMap::new())),
            audit_fail_once: Arc::new(Once::new()),
            board,
        }
    }

    fn active(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn start(&self, cmd: Command, num: usize) {
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

            let (progress, mut forwarder) = progress_forwarder(jobs.bridge.clone(), cmd.id.clone());
            let mut out = jobs
                .router
                .dispatch(
                    Ctx::new(token.clone()).with_progress(progress),
                    &cmd.kind,
                    cmd.extras.clone(),
                )
                .await;
            if let Some(timer) = timer {
                timer.abort();
            }
            if tokio::time::timeout(PROGRESS_DRAIN, &mut forwarder)
                .await
                .is_err()
            {
                forwarder.abort();
            }
            let stopped = token.is_cancelled() && !out.ok;
            let why_stopped = if !stopped {
                String::new()
            } else if timed_out.load(Ordering::SeqCst) {
                out.error = "command timed out".to_string();
                format!(
                    "It ran longer than the {} it was given.",
                    human_duration(Duration::from_millis(cmd.timeout_ms.max(0) as u64))
                )
            } else {
                out.error = "command cancelled".to_string();
                if jobs.root.is_cancelled() {
                    "The session was ending.".to_string()
                } else {
                    "The helper stopped it.".to_string()
                }
            };

            let ending = host::ending_for(&out, stopped);
            jobs.board.finish(&cmd.id, ending);
            let detail = match ending {
                Ending::Stopped => why_stopped,
                Ending::Refused => String::new(),
                _ => summarize_result(&out),
            };
            console::lines(&host::result_lines(
                num,
                &out,
                ending,
                jobs.board.read_only(),
                &detail,
                console::body_width(),
            ));

            jobs.write_audit(&cmd, &out, ending);
            match jobs
                .bridge
                .send_command_result_or_queue(&cmd.id, &out)
                .await
            {
                Ok(Delivery::Sent) => slog!(
                    "command result sent sid={} id={} ok={} elapsed_ms={}",
                    jobs.sid,
                    cmd.id,
                    out.ok,
                    out.elapsed_ms
                ),
                Ok(Delivery::Queued) => {
                    slog!("command result queued sid={} id={}", jobs.sid, cmd.id);
                    console::line(&format!(
                        "{}  The connection dropped before this result was sent. Handoff will send it when it reconnects, but the helper may need to run the command again.",
                        tag(num)
                    ));
                }
                Err(err) => {
                    slog!("send result failed sid={} id={}: {err}", jobs.sid, cmd.id);
                    console::line(&format!(
                        "{}  Handoff couldn't send this result to the helper: {}",
                        tag(num),
                        plain_error(&err)
                    ));
                }
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

    fn write_audit(&self, cmd: &Command, out: &Outcome, ending: Ending) {
        let Some(log) = &self.audit else {
            return;
        };
        let decision = audit_decision(
            capabilities::last_consent_decision(&cmd.kind),
            ending,
            category_for(&cmd.kind).is_some(),
        );
        let result = match ending {
            Ending::Refused => "denied",
            Ending::Done => "ok",
            _ => "err",
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
                console::eline(&format!(
                    "Handoff stopped keeping a record of this session: {}",
                    plain_error(&err.to_string())
                ));
            });
        }
    }
}

fn audit_decision(last: Decision, ending: Ending, gated: bool) -> Decision {
    if !gated {
        return Decision::NotRequired;
    }
    match ending {
        Ending::Refused => Decision::PromptDeny,
        _ if last == Decision::PromptDeny => Decision::PromptAllow,
        _ => last,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_decision_follows_what_happened_to_this_command() {
        assert_eq!(
            audit_decision(Decision::GrantCached, Ending::Refused, true),
            Decision::PromptDeny
        );
        assert_eq!(
            audit_decision(Decision::PromptDeny, Ending::Done, true),
            Decision::PromptAllow
        );
        assert_eq!(
            audit_decision(Decision::GrantCached, Ending::Done, true),
            Decision::GrantCached
        );
        assert_eq!(
            audit_decision(Decision::PromptDeny, Ending::Done, false),
            Decision::NotRequired
        );
    }

    #[test]
    fn progress_pieces_split_on_char_boundaries() {
        assert_eq!(split_at_chars("ab\u{e9}cd", 3), vec!["ab", "\u{e9}c", "d"]);
        assert_eq!(split_at_chars("", 3), Vec::<&str>::new());
        let mut s = "ab\u{e9}".to_string();
        truncate_at_char(&mut s, 3);
        assert_eq!(s, "ab");
    }

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

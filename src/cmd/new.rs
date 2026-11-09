use super::new_reconnect::reconnect_bridge;
use std::io::IsTerminal;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio_util::sync::CancellationToken;

use super::flags::{Def, FlagError, Options, parse_options};
use super::host::{self, Board, tag};
use super::keys::{ENDING, start_keyboard_control};
pub use super::new_jobs::Jobs;
pub use super::new_reconnect::ReconnectBackoff;
pub use super::new_reconnect::sleep_cancel;
use super::summary::describe_args;
use crate::audit::Logger;
use crate::consent::{self};
use crate::console::{self, Tone, paint, plain_error};
use crate::dispatch::Router;
use crate::relay::{self, Bridge, RecvError};
use crate::{VERSION, capabilities, stayawake, supportlog, util};

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
pub(super) const RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);
pub(super) const STILL_TRYING_AFTER: usize = 3;

pub const ENDED: &str = "Session ended. The helper can't reach this computer any more.";
pub const LOST: &str = "The connection to the Handoff server dropped. Handoff is reconnecting, so you don't need to do anything yet.";
pub const STILL_TRYING: &str = "Still trying to reconnect. Check that this computer is online. The helper's link will work again once Handoff is back.";
pub const RECONNECTED: &str = "Reconnected. The helper can carry on with the same link.";

pub fn new(args: &[String]) -> i32 {
    let (opts, parsed) = match parse_options("new", args, &NEW_FLAGS) {
        Ok(v) => v,
        Err(FlagError::Help) => return 2,
        Err(err) => {
            crate::errln!("{err}");
            return 2;
        }
    };
    let consent_mode = parsed.string("consent");
    let no_keep_awake = parsed.boolean("no-keep-awake");
    match consent_mode.as_str() {
        "ask" => {}
        "deny" => capabilities::deny_all_risky(),
        _ => {
            crate::errln!("--consent can only be ask or deny");
            return 2;
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            crate::errln!("{err}");
            return 1;
        }
    };
    let mode = SessionMode {
        read_only: consent_mode == "deny",
        no_keep_awake,
    };
    let code = runtime.block_on(session(opts, mode));
    runtime.shutdown_timeout(Duration::from_secs(2));
    code
}

pub fn short_sid(token: &str) -> String {
    token.get(..11).unwrap_or(token).to_string()
}

#[derive(Clone, Copy)]
struct Welcome {
    read_only: bool,
    keep_awake: bool,
    copied: bool,
}

#[derive(Clone, Copy)]
pub(super) struct SessionMode {
    pub(super) read_only: bool,
    pub(super) no_keep_awake: bool,
}

fn print_welcome(board: &Board, welcome: Welcome) {
    let Welcome {
        read_only,
        keep_awake,
        copied,
    } = welcome;
    crate::outln!();
    crate::outln!(
        "{}",
        paint(Tone::Bold, "Send this link to the person helping you:")
    );
    crate::outln!("   {}", paint(Tone::Cyan, board.link()));
    if copied {
        crate::outln!("The link is copied, so you can paste it straight into a message.");
    }
    crate::outln!();
    if read_only {
        crate::outln!(
            "This session can only look. The helper can't change anything on this computer."
        );
    } else {
        crate::outln!("If the helper wants to change something, Handoff will ask you first.");
    }
    if keep_awake {
        crate::outln!("This computer will stay awake until the session ends.");
    }
    crate::outln!("Type q and press Enter to end the session, or ? for other options.");
    crate::outln!();
}

pub(super) async fn session(opts: Options, mode: SessionMode) -> i32 {
    let SessionMode {
        read_only,
        no_keep_awake,
    } = mode;
    console::init();
    let relay_base = opts.relay.clone();
    tracing::info!("session start relay={} version={}", relay_base, VERSION);

    let root = CancellationToken::new();

    let log = match Logger::new_in_dir(&opts.audit_dir) {
        Ok(l) => Some(Arc::new(l)),
        Err(err) => {
            tracing::info!("audit log unavailable: {err}");
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
    tracing::info!("mint ok sid={sid}");

    let board = Arc::new(Board::new(&mint.view_url, read_only));
    if opts.verbose {
        let log_path = supportlog::path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        crate::outln!("Server:          {relay_base}");
        crate::outln!("Support log:     {log_path}");
        if let Some(log) = &log {
            crate::outln!("Activity record: {}", log.dir().display());
        }
    }
    let keep_awake = !no_keep_awake && opts.keep_awake;
    let copied = std::io::stdout().is_terminal() && host::copy_to_clipboard(board.link());
    tracing::info!("link copied={copied}");
    print_welcome(
        &board,
        Welcome {
            read_only,
            keep_awake,
            copied,
        },
    );

    {
        let root = root.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("shutdown requested by signal");
                crate::outln!();
                console::line(ENDING);
                root.cancel();
            }
        });
    }

    if keep_awake {
        stayawake::start(root.clone());
    }
    start_keyboard_control(root.clone(), log.clone(), Arc::clone(&board));

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
    tracing::info!("bridge connected sid={sid}");

    let mut router = Router::new();
    capabilities::register_all(&mut router, Some(bridge.clone()));
    let router = Arc::new(router);
    let kinds = router.kinds();
    let specs = router.specs();
    board.learn_labels(&specs);
    board.watch_consent();
    tracing::info!("capabilities registered count={}", kinds.len());

    let hostname = util::hostname();
    bridge.want_presence();
    if let Err(err) = bridge.send_hello(&hostname, VERSION, &kinds, &specs).await {
        tracing::info!("hello failed sid={sid}: {err}");
        console::eline(&format!(
            "Couldn't finish connecting to the Handoff server: {}. Try again in a moment.",
            plain_error(&err)
        ));
        consent::set_observer(None);
        bridge.close().await;
        return 1;
    }
    tracing::info!("hello sent sid={sid} hostname={hostname}");
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
        Arc::clone(&board),
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
    tracing::info!("mint failed: {err}");
    console::eline(&format!(
        "Couldn't start a session. Handoff couldn't reach the server at {relay_base}: {}. Check that this computer is online, then try again.",
        plain_error(err)
    ));
    1
}

fn dial_failed(sid: &str, err: &str) -> i32 {
    tracing::info!("bridge dial failed sid={sid}: {err}");
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
            tracing::info!(
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
                tracing::info!("recv ended sid={sid} err={err}");
                return false;
            }
            tracing::info!("recv ended sid={sid} err={err}; reconnecting");
            console::line(&paint(Tone::Yellow, LOST));
            return reconnect_bridge(cancel, sid, bridge, backoff).await;
        }
    };
    backoff.reset();
    tracing::info!("command received sid={sid} id={} kind={}", cmd.id, cmd.kind);

    if cmd.kind == relay::PRESENCE_KIND {
        let viewers = cmd
            .extras
            .get("viewers")
            .and_then(|v| v.as_u64())
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        tracing::info!("presence sid={sid} viewers={viewers}");
        let generation = jobs.board.see_viewers(viewers);
        let board = Arc::clone(&jobs.board);
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
            tracing::info!("cancel control missing target sid={sid} id={}", cmd.id);
            return true;
        }
        if jobs.cancel(&target) {
            match jobs.board.number_of(&target) {
                Some(num) => {
                    console::line(&format!("{}  The helper asked to stop this.", tag(num)))
                }
                None => console::line("The helper asked to stop a command."),
            }
            tracing::info!("command cancel requested sid={sid} id={target}");
        } else {
            tracing::info!("cancel target not active sid={sid} id={target}");
        }
        return true;
    }
    if capabilities::is_tunnel_frame_kind(&cmd.kind) {
        capabilities::enqueue_tunnel_frame(&cmd.kind, &cmd.extras);
        return true;
    }

    let view = describe_args(&cmd.extras);
    let num = jobs.board.begin(&cmd.id, &cmd.kind);
    tracing::info!("command number sid={sid} id={} num={num}", cmd.id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::host::Ending;
    use crate::cmd::new_jobs::audit_decision;
    use crate::cmd::new_progress::split_at_chars;
    use crate::cmd::new_progress::truncate_at_char;
    use crate::consent_policy::Decision;

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

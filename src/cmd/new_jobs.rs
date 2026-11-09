use super::host::{self, Board, Ending, tag};
use super::new_progress::PROGRESS_DRAIN;
use super::new_progress::progress_forwarder;
use super::summary::summarize_result;
use crate::audit::{self, Logger};
use crate::capabilities;
use crate::consent::{Decision, category_for};
use crate::console::{self, human_duration, plain_error};
use crate::dispatch::{Ctx, Outcome, Router};
use crate::relay::{Bridge, Command, Delivery};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Jobs {
    root: CancellationToken,
    sid: String,
    router: Arc<Router>,
    bridge: Bridge,
    audit: Option<Arc<Logger>>,
    active: Arc<Mutex<HashMap<String, CancellationToken>>>,
    audit_fail_once: Arc<Once>,
    pub(super) board: Arc<Board>,
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
                active: Arc::clone(&jobs.active),
                id: cmd.id.clone(),
                token: token.clone(),
            };

            let timed_out = Arc::new(AtomicBool::new(false));
            let timer = (cmd.timeout_ms > 0).then(|| {
                let token = token.clone();
                let timed_out = Arc::clone(&timed_out);
                let after =
                    Duration::from_millis(u64::try_from(cmd.timeout_ms).unwrap_or(u64::MAX));
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
                    human_duration(Duration::from_millis(
                        u64::try_from(cmd.timeout_ms).unwrap_or(0)
                    ))
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
                Ok(Delivery::Sent) => tracing::info!(
                    "command result sent sid={} id={} ok={} elapsed_ms={}",
                    jobs.sid,
                    cmd.id,
                    out.ok,
                    out.elapsed_ms
                ),
                Ok(Delivery::Queued) => {
                    tracing::info!("command result queued sid={} id={}", jobs.sid, cmd.id);
                    console::line(&format!(
                        "{}  The connection dropped before this result was sent. Handoff will send it when it reconnects, but the helper may need to run the command again.",
                        tag(num)
                    ));
                }
                Err(err) => {
                    tracing::info!("send result failed sid={} id={}: {err}", jobs.sid, cmd.id);
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
                tracing::info!("audit write failed sid={}: {err}", self.sid);
                console::eline(&format!(
                    "Handoff stopped keeping a record of this session: {}",
                    plain_error(&err.to_string())
                ));
            });
        }
    }
}

pub(super) fn audit_decision(last: Decision, ending: Ending, gated: bool) -> Decision {
    if !gated {
        return Decision::NotRequired;
    }
    match ending {
        Ending::Refused => Decision::PromptDeny,
        _ if last == Decision::PromptDeny => Decision::PromptAllow,
        _ => last,
    }
}

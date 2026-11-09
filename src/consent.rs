use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

pub use super::consent_ledger::Grant;
pub use super::consent_ledger::Ledger;
pub use super::consent_ledger::scope_text;
pub use super::consent_policy::Ask;
pub use super::consent_policy::CONSENT_ARG;
pub use super::consent_policy::Category;
pub use super::consent_policy::Decision;
pub use super::consent_policy::GATED_KINDS;
pub use super::consent_policy::MAX_CONSENT_MINUTES;
pub use super::consent_policy::Term;
pub use super::consent_policy::category_for;
pub use super::consent_policy::grant_ttl;
pub use super::consent_policy::take_ask;
pub use super::consent_policy::term_for;
pub use super::consent_prompt::PROMPT_TITLE;
pub use super::consent_prompt::PromptParts;
pub use super::consent_prompt::countdown_text;
pub use super::consent_prompt::prompt_parts;
pub use super::consent_prompt::prompt_text;
pub use super::consent_prompt::system_prompt;
pub use super::consent_prompt::typed_answer;
pub use super::consent_prompt::typed_question;

pub const EXEC_GRANT_TTL: Duration = Duration::from_secs(15 * 60);
pub const FILES_GRANT_TTL: Duration = Duration::from_secs(15 * 60);
pub const DENY_BACKOFF: Duration = Duration::from_secs(60);
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
pub(super) const PROMPT_CLOSE_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    No,
    Once,
    ForGrant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentError {
    Denied,
    Cancelled,
    TimedOut,
    Other(String),
}

impl fmt::Display for ConsentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConsentError::Denied => f.write_str("the host declined this request"),
            ConsentError::Cancelled => {
                f.write_str("the request was cancelled before anyone answered")
            }
            ConsentError::TimedOut => {
                f.write_str("nobody answered the permission question in time")
            }
            ConsentError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ConsentError {}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub kind: String,
    pub category: Option<Category>,
    pub summary: String,
    pub detail: Option<String>,
    pub ask: Ask,
}

impl Request {
    pub fn for_kind(kind: &str, summary: &str) -> Request {
        Request {
            kind: kind.to_string(),
            summary: summary.to_string(),
            ..Request::default()
        }
    }

    pub fn asking(mut self, ask: Ask) -> Request {
        self.ask = ask;
        self
    }

    pub fn showing(mut self, detail: Option<&str>) -> Request {
        self.detail = detail.map(str::to_string);
        self
    }

    pub fn term(&self) -> Term {
        match self.category.or_else(|| category_for(&self.kind)) {
            Some(c) => term_for(c, self.ask),
            None => Term::Session,
        }
    }
}

pub type Prompt = Arc<
    dyn Fn(CancellationToken, Request) -> BoxFuture<'static, Result<Answer, ConsentError>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Asking,
    Allowed(Term),
    Denied,
    TimedOut,
    Withdrawn,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Event {
    pub request: Request,
    pub step: Step,
}

pub type Observer = Arc<dyn Fn(&Event) + Send + Sync>;

static OBSERVER: Mutex<Option<Observer>> = Mutex::new(None);

pub fn set_observer(observer: Option<Observer>) {
    *OBSERVER.lock().unwrap_or_else(|e| e.into_inner()) = observer;
}

pub(super) fn tell(request: &Request, step: Step) {
    let observer = OBSERVER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(observer) = observer {
        observer(&Event {
            request: request.clone(),
            step,
        });
    }
}

#[cfg(windows)]
#[path = "consent_dialog.rs"]
pub mod dialog;

#[cfg(windows)]
#[path = "consent_dialog_layout.rs"]
mod dialog_layout;

#[cfg(windows)]
#[path = "consent_dialog_proc.rs"]
mod dialog_proc;

#[cfg(test)]
#[cfg(windows)]
#[path = "consent_preview.rs"]
mod preview;

#[cfg(test)]
#[cfg(windows)]
#[path = "consent_preview_capture.rs"]
mod preview_capture;

#[cfg(test)]
#[cfg(windows)]
#[path = "consent_preview_inspect.rs"]
mod preview_inspect;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consent_prompt::CUT_NOTE;
    use crate::consent_prompt::DETAIL_LINES;
    use serde_json::Map;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    fn counting(answer: Answer, delay_ms: u64) -> (Prompt, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let prompt: Prompt = Arc::new(move |_, _| {
            let c = Arc::clone(&c);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                Ok(answer)
            })
        });
        (prompt, calls)
    }

    fn tok() -> CancellationToken {
        CancellationToken::new()
    }

    fn req(kind: &str) -> Request {
        Request::for_kind(kind, "")
    }

    #[tokio::test]
    async fn grant_in_one_category_does_not_cover_another() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        for kind in ["pico.reset", "ps.exec", "fs.delete"] {
            assert!(l.require(&tok(), req(kind)).await.1.is_ok(), "{kind}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn second_command_in_the_same_category_reuses_the_grant() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        for kind in ["fs.delete", "fs.mkdir", "fs.upload"] {
            assert!(l.require(&tok(), req(kind)).await.1.is_ok(), "{kind}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_requests_in_one_category_prompt_once() {
        let (p, calls) = counting(Answer::ForGrant, 25);
        let l = Arc::new(Ledger::new(p));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let l = Arc::clone(&l);
            tasks.push(tokio::spawn(async move {
                l.require(&CancellationToken::new(), Request::for_kind("ps.exec", ""))
                    .await
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap().1.is_ok());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_requests_in_different_categories_prompt_separately() {
        let (p, calls) = counting(Answer::ForGrant, 10);
        let l = Arc::new(Ledger::new(p));
        let mut tasks = Vec::new();
        for kind in ["ps.exec", "fs.delete", "svc.control"] {
            let l = Arc::clone(&l);
            tasks.push(tokio::spawn(async move {
                l.require(&CancellationToken::new(), Request::for_kind(kind, ""))
                    .await
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap().1.is_ok());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn denial_is_not_terminal_for_the_session() {
        let (p, calls) = counting(Answer::No, 0);
        let l = Ledger::new(p);
        assert_eq!(
            l.require(&tok(), req("fs.delete")).await.1,
            Err(ConsentError::Denied)
        );
        assert_eq!(
            l.require(&tok(), req("fs.delete")).await.1,
            Err(ConsentError::Denied)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        l.advance(DENY_BACKOFF + Duration::from_secs(1));
        assert_eq!(
            l.require(&tok(), req("fs.delete")).await.1,
            Err(ConsentError::Denied)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exec_grant_expires() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        assert!(l.require(&tok(), req("ps.exec")).await.1.is_ok());
        l.advance(EXEC_GRANT_TTL - Duration::from_secs(60));
        assert!(l.require(&tok(), req("ps.exec")).await.1.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        l.advance(Duration::from_secs(120));
        assert!(l.require(&tok(), req("ps.exec")).await.1.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn power_is_never_remembered() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        for _ in 0..3 {
            assert!(l.require(&tok(), req("sys.restart")).await.1.is_ok());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn revoke_all_forces_a_fresh_prompt() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        assert!(l.require(&tok(), req("fs.delete")).await.1.is_ok());
        assert_eq!(l.revoke_all(), 1);
        assert!(l.grants().is_empty());
        assert!(l.require(&tok(), req("fs.delete")).await.1.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn deny_everything_refuses_without_prompting() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        l.deny_everything();
        for kind in ["ps.exec", "fs.delete", "tunnel.open"] {
            assert_eq!(
                l.require(&tok(), req(kind)).await.1,
                Err(ConsentError::Denied),
                "{kind}"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn ungated_kind_needs_no_consent() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        let (decision, res) = l.require(&tok(), req("sys.info")).await;
        assert!(res.is_ok());
        assert_eq!(decision, Decision::NotRequired);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn decisions_are_reported_for_the_audit_log() {
        let (p, _) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        assert_eq!(
            l.require(&tok(), req("fs.delete")).await.0,
            Decision::PromptAllowFor(15)
        );
        assert_eq!(
            l.require(&tok(), req("fs.mkdir")).await.0,
            Decision::GrantCached
        );
    }

    #[tokio::test]
    async fn dropped_prompt_releases_the_category() {
        let (slow, _) = counting(Answer::ForGrant, 5_000);
        let l = Arc::new(Ledger::new(slow));
        let l2 = Arc::clone(&l);
        let task = tokio::spawn(async move {
            l2.require(&CancellationToken::new(), Request::for_kind("ps.exec", ""))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        task.abort();
        let _ = task.await;
        assert!(!l.lock().asking.contains(&Category::Exec));
    }

    #[test]
    fn every_gated_kind_has_a_category() {
        for (kind, category) in GATED_KINDS {
            assert!(!category.label().is_empty(), "{kind}");
            assert_eq!(category_for(kind), Some(category));
        }
        for category in Category::ALL {
            assert_ne!(category.as_str(), "");
        }
    }

    #[test]
    fn prompt_text_strips_hidden_characters_from_any_summary() {
        let text = prompt_text(&Request {
            kind: "svc.control".into(),
            category: Some(Category::Services),
            summary: "Stop \u{202E}exe.ssecorp\u{202C} spooler\u{200B}\u{7}".into(),
            ..Request::default()
        });
        assert!(text.contains("Stop exe.ssecorp spooler"), "{text}");
        for c in ['\u{202E}', '\u{202C}', '\u{200B}', '\u{7}'] {
            assert!(!text.contains(c));
        }

        let text = prompt_text(&Request {
            kind: "svc.control".into(),
            category: Some(Category::Services),
            summary: "\u{200B}\u{202E}".into(),
            ..Request::default()
        });
        assert!(text.contains("Run a command that can change this computer."));

        let text = prompt_text(&Request {
            kind: "svc.control".into(),
            category: Some(Category::Services),
            summary: "x\n".repeat(80),
            ..Request::default()
        });
        assert!(text.contains("(The rest is too long to show here.)"));
    }

    #[test]
    fn prompt_text_names_the_scope_and_what_is_granted() {
        let text = prompt_text(&Request {
            kind: "ps.exec".into(),
            category: Some(Category::Exec),
            summary: "Run: Get-Date".into(),
            ..Request::default()
        });
        assert!(text.contains("Run: Get-Date"));
        assert!(text.contains("for the next 15 minutes"));
        assert!(text.contains("run PowerShell commands"));

        let text = prompt_text(&Request {
            kind: "sys.restart".into(),
            category: Some(Category::Power),
            summary: String::new(),
            ..Request::default()
        });
        assert!(text.contains("only this request goes ahead"));
        assert!(text.contains("Run a command that can change this computer."));
        assert!(text.contains("within 2 minutes"));

        let text = prompt_text(&Request {
            kind: "fs.delete".into(),
            category: Some(Category::Files),
            summary: "Delete C:\\x".into(),
            ..Request::default()
        });
        assert!(text.contains("read, add, change, and delete your files for the next 15 minutes"));

        let text = prompt_text(&Request {
            kind: "tunnel.open".into(),
            category: Some(Category::Network),
            summary: String::new(),
            ..Request::default()
        });
        assert!(text.contains("until this session ends"));
    }

    #[tokio::test]
    async fn files_grant_expires_after_fifteen_minutes() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        assert_eq!(
            l.require(&tok(), req("fs.delete")).await.0,
            Decision::PromptAllowFor(15)
        );
        l.advance(FILES_GRANT_TTL - Duration::from_secs(60));
        assert_eq!(
            l.require(&tok(), req("fs.mkdir")).await.0,
            Decision::GrantCached
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(l.grants().iter().all(|g| g.expires()));

        l.advance(Duration::from_secs(120));
        assert!(l.grants().is_empty());
        assert_eq!(
            l.require(&tok(), req("fs.upload")).await.0,
            Decision::PromptAllowFor(15)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    fn waits_for_cancel() -> (Prompt, Arc<AtomicUsize>) {
        let closed = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&closed);
        let prompt: Prompt = Arc::new(move |cancel: CancellationToken, _| {
            let c = Arc::clone(&c);
            Box::pin(async move {
                cancel.cancelled().await;
                c.fetch_add(1, Ordering::SeqCst);
                Err(ConsentError::Cancelled)
            })
        });
        (prompt, closed)
    }

    #[tokio::test]
    async fn unanswered_prompt_is_closed_and_refused_after_the_timeout() {
        let (p, closed) = waits_for_cancel();
        let l = Ledger::with_timeout(p, Duration::from_millis(50));
        let (decision, res) = within(l.require(&tok(), req("ps.exec"))).await;
        assert_eq!(decision, Decision::PromptDeny);
        assert_eq!(res, Err(ConsentError::TimedOut));
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        assert!(!l.lock().denied.contains_key(&Category::Exec));
        assert!(!l.lock().asking.contains(&Category::Exec));
    }

    #[tokio::test]
    async fn cancelling_the_command_closes_the_prompt() {
        let (p, closed) = waits_for_cancel();
        let l = Arc::new(Ledger::new(p));
        let cancel = tok();
        let l2 = Arc::clone(&l);
        let c2 = cancel.clone();
        let task = tokio::spawn(async move { l2.require(&c2, req("fs.delete")).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        let (decision, res) = within(task).await.unwrap();
        assert_eq!(decision, Decision::PromptDeny);
        assert_eq!(res, Err(ConsentError::Cancelled));
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        assert!(!l.lock().asking.contains(&Category::Files));
    }

    #[tokio::test]
    async fn prompt_that_ignores_cancel_does_not_hang_the_command() {
        let (slow, _) = counting(Answer::ForGrant, 60_000);
        let l = Ledger::new(slow);
        let cancel = tok();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            c2.cancel();
        });
        let started = Instant::now();
        let (_, res) = l.require(&cancel, req("ps.exec")).await;
        assert_eq!(res, Err(ConsentError::Cancelled));
        assert!(started.elapsed() < PROMPT_CLOSE_WAIT + Duration::from_secs(1));
    }

    #[tokio::test]
    async fn observer_hears_the_question_and_the_answer() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = Arc::clone(&seen);
        set_observer(Some(Arc::new(move |e: &Event| {
            if e.request.summary == "observer-test" {
                s2.lock().unwrap().push(e.step.clone());
            }
        })));
        let (p, _) = counting(Answer::No, 0);
        let l = Ledger::new(p);
        let _ = l
            .require(&tok(), Request::for_kind("proc.kill", "observer-test"))
            .await;
        set_observer(None);
        assert_eq!(*seen.lock().unwrap(), vec![Step::Asking, Step::Denied]);
    }

    fn asked(kind: &str, ask: Ask) -> Request {
        Request::for_kind(kind, "").asking(ask)
    }

    fn args_of(v: serde_json::Value) -> Map<String, Value> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn consent_minutes_is_parsed_and_removed() {
        for (v, want) in [
            (serde_json::json!({}), Ask::Default),
            (serde_json::json!({"consent_minutes": null}), Ask::Default),
            (serde_json::json!({"consent_minutes": 0}), Ask::Once),
            (serde_json::json!({"consent_minutes": 1}), Ask::Minutes(1)),
            (
                serde_json::json!({"consent_minutes": 240}),
                Ask::Minutes(240),
            ),
        ] {
            let mut args = args_of(v.clone());
            args.insert("script".into(), Value::from("Get-Date"));
            assert_eq!(take_ask("ps.exec", &mut args), Ok(want), "{v}");
            assert!(!args.contains_key(CONSENT_ARG));
            assert_eq!(args["script"], Value::from("Get-Date"));
        }
    }

    #[test]
    fn bad_consent_minutes_are_refused_with_a_plain_error() {
        for bad in [
            serde_json::json!(-1),
            serde_json::json!(241),
            serde_json::json!(15.5),
            serde_json::json!(15.0),
            serde_json::json!("15"),
            serde_json::json!(true),
            serde_json::json!(u64::MAX),
        ] {
            let mut args = args_of(serde_json::json!({ "consent_minutes": bad }));
            assert_eq!(
                take_ask("ps.exec", &mut args),
                Err("ps.exec: 'consent_minutes' must be a whole number from 0 to 240 (0 means just this once)".to_string()),
                "{bad}"
            );
        }
    }

    #[test]
    fn terms_follow_the_category_and_the_ask() {
        assert_eq!(term_for(Category::Exec, Ask::Default), Term::Minutes(15));
        assert_eq!(term_for(Category::Files, Ask::Default), Term::Minutes(15));
        assert_eq!(term_for(Category::Network, Ask::Default), Term::Session);
        assert_eq!(term_for(Category::Power, Ask::Default), Term::Once);
        assert_eq!(term_for(Category::Exec, Ask::Once), Term::Once);
        assert_eq!(
            term_for(Category::Exec, Ask::Minutes(90)),
            Term::Minutes(90)
        );
        assert_eq!(
            term_for(Category::Network, Ask::Minutes(5)),
            Term::Minutes(5)
        );
        assert_eq!(term_for(Category::Power, Ask::Minutes(60)), Term::Once);
    }

    #[test]
    fn terms_read_naturally() {
        assert_eq!(Term::Once.text(), "just this once");
        assert_eq!(Term::Minutes(1).text(), "for the next minute");
        assert_eq!(Term::Minutes(30).text(), "for the next 30 minutes");
        assert_eq!(Term::Minutes(60).text(), "for the next hour");
        assert_eq!(Term::Minutes(90).text(), "for the next 90 minutes");
        assert_eq!(Term::Minutes(240).text(), "for the next 4 hours");
        assert_eq!(Term::Session.text(), "until this session ends");
        assert_eq!(Term::Once.button(), None);
        assert_eq!(Term::Minutes(1).button().unwrap(), "Allow for 1 minute");
        assert_eq!(Term::Minutes(15).button().unwrap(), "Allow for 15 minutes");
        assert_eq!(Term::Minutes(60).button().unwrap(), "Allow for 1 hour");
        assert_eq!(Term::Minutes(120).button().unwrap(), "Allow for 2 hours");
        assert_eq!(
            Term::Session.button().unwrap(),
            "Allow until this session ends"
        );
        assert_eq!(scope_text(Some(Category::Exec)), "for the next 15 minutes");
        assert_eq!(scope_text(Some(Category::Power)), "just this once");
    }

    #[test]
    fn audit_strings_name_the_grant() {
        assert_eq!(Decision::PromptAllow.as_str(), "prompt_allow");
        assert_eq!(Decision::PromptAllowOnce.as_str(), "prompt_allow_once");
        assert_eq!(Decision::PromptAllowFor(30).as_str(), "prompt_allow_30m");
    }

    #[tokio::test]
    async fn one_off_request_is_not_remembered() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        for _ in 0..3 {
            let (decision, res) = l.require(&tok(), asked("ps.exec", Ask::Once)).await;
            assert!(res.is_ok());
            assert_eq!(decision, Decision::PromptAllowOnce);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(l.grants().is_empty());
    }

    #[tokio::test]
    async fn host_can_shorten_a_grant_to_one_time() {
        let (p, calls) = counting(Answer::Once, 0);
        let l = Ledger::new(p);
        for _ in 0..2 {
            assert_eq!(
                l.require(&tok(), req("fs.delete")).await,
                (Decision::PromptAllowOnce, Ok(()))
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(l.grants().is_empty());
    }

    #[tokio::test]
    async fn asked_length_sets_when_the_grant_runs_out() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        assert_eq!(
            l.require(&tok(), asked("ps.exec", Ask::Minutes(30)))
                .await
                .0,
            Decision::PromptAllowFor(30)
        );
        l.advance(Duration::from_secs(29 * 60));
        assert_eq!(
            l.require(&tok(), req("ps.exec")).await.0,
            Decision::GrantCached
        );
        l.advance(Duration::from_secs(2 * 60));
        assert!(l.grants().is_empty());
        assert_eq!(
            l.require(&tok(), req("ps.exec")).await.0,
            Decision::PromptAllowFor(15)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let (p, _) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        assert_eq!(
            l.require(&tok(), asked("tunnel.open", Ask::Minutes(5)))
                .await
                .0,
            Decision::PromptAllowFor(5)
        );
        assert!(l.grants().iter().all(|g| g.expires()));
        l.advance(Duration::from_secs(5 * 60));
        assert!(l.grants().is_empty());
    }

    #[tokio::test]
    async fn asking_for_more_than_is_left_asks_again() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        assert!(l.require(&tok(), req("ps.exec")).await.1.is_ok());
        l.advance(Duration::from_secs(10 * 60));

        for ask in [Ask::Default, Ask::Once, Ask::Minutes(3)] {
            assert_eq!(
                l.require(&tok(), asked("ps.exec", ask)).await.0,
                Decision::GrantCached,
                "{ask:?}"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        assert_eq!(
            l.require(&tok(), asked("ps.exec", Ask::Minutes(10)))
                .await
                .0,
            Decision::PromptAllowFor(10)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        l.advance(Duration::from_secs(9 * 60));
        assert_eq!(
            l.require(&tok(), req("ps.exec")).await.0,
            Decision::GrantCached
        );
    }

    #[tokio::test]
    async fn declining_a_longer_grant_keeps_the_one_already_given() {
        let answers = Arc::new(Mutex::new(vec![Answer::No, Answer::ForGrant]));
        let p: Prompt = Arc::new(move |_, _| {
            let next = answers.lock().unwrap().pop().unwrap();
            Box::pin(async move { Ok(next) })
        });
        let l = Ledger::new(p);
        assert!(l.require(&tok(), req("ps.exec")).await.1.is_ok());
        assert_eq!(
            l.require(&tok(), asked("ps.exec", Ask::Minutes(60))).await,
            (Decision::PromptDeny, Err(ConsentError::Denied))
        );
        assert_eq!(
            l.require(&tok(), req("ps.exec")).await.0,
            Decision::GrantCached
        );
        assert_eq!(l.grants().len(), 1);
    }

    #[tokio::test]
    async fn session_grant_covers_a_timed_ask() {
        let (p, calls) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        assert_eq!(
            l.require(&tok(), req("tunnel.open")).await.0,
            Decision::PromptAllow
        );
        assert_eq!(
            l.require(&tok(), asked("tunnel.open", Ask::Minutes(240)))
                .await
                .0,
            Decision::GrantCached
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn power_stays_one_time_whatever_is_asked() {
        let terms = Arc::new(Mutex::new(Vec::new()));
        let t2 = Arc::clone(&terms);
        let p: Prompt = Arc::new(move |_, req: Request| {
            t2.lock().unwrap().push(req.term());
            Box::pin(async { Ok(Answer::ForGrant) })
        });
        let l = Ledger::new(p);
        for ask in [Ask::Default, Ask::Minutes(60), Ask::Once] {
            assert_eq!(
                l.require(&tok(), asked("sys.restart", ask)).await,
                (Decision::PromptAllowOnce, Ok(()))
            );
        }
        assert!(l.grants().is_empty());
        assert_eq!(*terms.lock().unwrap(), vec![Term::Once; 3]);
    }

    #[tokio::test]
    async fn observer_hears_what_the_host_picked() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = Arc::clone(&seen);
        set_observer(Some(Arc::new(move |e: &Event| {
            if e.request.summary == "picked-test" {
                s2.lock().unwrap().push(e.step.clone());
            }
        })));
        let (p, _) = counting(Answer::ForGrant, 0);
        let l = Ledger::new(p);
        let _ = l
            .require(
                &tok(),
                Request::for_kind("ps.exec", "picked-test").asking(Ask::Minutes(45)),
            )
            .await;
        let (p, _) = counting(Answer::Once, 0);
        let l = Ledger::new(p);
        let _ = l
            .require(&tok(), Request::for_kind("fs.delete", "picked-test"))
            .await;
        set_observer(None);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                Step::Asking,
                Step::Allowed(Term::Minutes(45)),
                Step::Asking,
                Step::Allowed(Term::Once)
            ]
        );
    }

    fn exec_request(summary: String, ask: Ask) -> Request {
        Request {
            kind: "ps.exec".into(),
            category: Some(Category::Exec),
            summary,
            ask,
            ..Request::default()
        }
    }

    #[test]
    fn prompt_parts_keep_the_command_apart_from_the_explanation() {
        let script = (1..=30)
            .map(|i| format!("Write-Output 'line {i}'"))
            .collect::<Vec<_>>()
            .join("\n");
        let parts = prompt_parts(&exec_request(
            format!("Runs this PowerShell script.\n\n{script}"),
            Ask::Minutes(30),
        ));
        assert_eq!(parts.summary, "Runs this PowerShell script.");
        let command = parts.command.as_deref().unwrap();
        assert!(command.starts_with("Write-Output 'line 1'\nWrite-Output 'line 2'\n"));
        assert_eq!(parts.detail, parts.command);
        assert_eq!(parts.grant_button.as_deref(), Some("Allow for 30 minutes"));
        let scope = parts.scope_line();
        assert!(scope.contains("run PowerShell commands for the next 30 minutes"));
        assert!(scope.contains("Choose Just this once"));
        assert!(!scope.contains("line 1"));

        let parts = prompt_parts(&exec_request(
            "Runs this PowerShell script.\n\nGet-Date".into(),
            Ask::Once,
        ));
        assert_eq!(parts.command.as_deref(), Some("Get-Date"));
        assert_eq!(parts.grant_button, None);
        assert!(parts.scope_line().contains("only this request goes ahead"));
        assert!(!parts.scope_line().contains("Choose Just this once"));

        let parts = prompt_parts(&Request {
            kind: "fs.mkdir".into(),
            category: Some(Category::Files),
            summary: "Creates the folder C:\\x on this computer.".into(),
            ask: Ask::Default,
            ..Request::default()
        });
        assert_eq!(parts.command, None);
        assert_eq!(parts.detail, None);
        assert_eq!(parts.grant_button.as_deref(), Some("Allow for 15 minutes"));
    }

    #[test]
    fn prompt_detail_holds_the_whole_text_without_hidden_characters() {
        let script = format!("{}\u{202E}tail\u{200B}end", "x".repeat(10_000));
        let req = exec_request(
            crate::capabilities::ps::consent_summary(&script),
            Ask::Default,
        )
        .showing(Some(&script));
        let parts = prompt_parts(&req);
        let detail = parts.detail.unwrap();
        assert_eq!(detail, format!("{}tailend", "x".repeat(10_000)));
        assert!(parts.command.unwrap().chars().count() < 2_100);
        assert!(parts.summary.starts_with("Runs this PowerShell script"));

        let lines: String = (1..=2000).map(|i| format!("Write-Output {i}\n")).collect();
        let parts = prompt_parts(
            &exec_request(
                crate::capabilities::ps::consent_summary(&lines),
                Ask::Default,
            )
            .showing(Some(&lines)),
        );
        let detail = parts.detail.unwrap();
        assert_eq!(detail.lines().count(), 2000);
        assert!(detail.ends_with("Write-Output 2000"));

        let huge = "y\n".repeat(DETAIL_LINES + 5);
        let parts =
            prompt_parts(&exec_request("Runs it.".into(), Ask::Default).showing(Some(&huge)));
        assert!(parts.detail.unwrap().ends_with(CUT_NOTE));
    }

    #[test]
    fn prompt_text_names_the_asked_length() {
        let text = prompt_text(&exec_request("Get-Date".into(), Ask::Minutes(120)));
        assert!(
            text.contains("run PowerShell commands for the next 2 hours"),
            "{text}"
        );
        let text = prompt_text(&exec_request("Get-Date".into(), Ask::Once));
        assert!(text.contains("only this request goes ahead"), "{text}");
    }

    #[test]
    fn countdown_counts_whole_seconds_up() {
        assert_eq!(
            countdown_text(Duration::from_secs(120)),
            "If you don't answer, Handoff says no for you in 2:00."
        );
        assert_eq!(
            countdown_text(Duration::from_millis(59_001)),
            "If you don't answer, Handoff says no for you in 1:00."
        );
        assert_eq!(
            countdown_text(Duration::from_millis(9_000)),
            "If you don't answer, Handoff says no for you in 0:09."
        );
    }

    #[test]
    fn typed_answers_map_to_the_choices() {
        let grant = Term::Minutes(15);
        assert_eq!(typed_answer("y\n", grant), Answer::ForGrant);
        assert_eq!(typed_answer(" Yes ", grant), Answer::ForGrant);
        assert_eq!(typed_answer("o", grant), Answer::Once);
        assert_eq!(typed_answer("", grant), Answer::No);
        assert_eq!(typed_answer("n", grant), Answer::No);
        assert_eq!(typed_answer("y", Term::Once), Answer::Once);
        assert!(typed_question(grant).contains("or o to allow just this once"));
        assert!(!typed_question(Term::Once).contains("or o"));
    }

    async fn within<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("timed out")
    }
}

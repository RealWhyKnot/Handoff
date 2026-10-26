use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::dispatch::for_display;

pub const EXEC_GRANT_TTL: Duration = Duration::from_secs(15 * 60);
pub const FILES_GRANT_TTL: Duration = Duration::from_secs(15 * 60);
pub const DENY_BACKOFF: Duration = Duration::from_secs(60);
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
const PROMPT_CLOSE_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Files,
    Processes,
    Services,
    Devices,
    Network,
    Capture,
    Exec,
    Power,
}

impl Category {
    pub const ALL: [Category; 8] = [
        Category::Files,
        Category::Processes,
        Category::Services,
        Category::Devices,
        Category::Network,
        Category::Capture,
        Category::Exec,
        Category::Power,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Files => "files",
            Category::Processes => "processes",
            Category::Services => "services",
            Category::Devices => "devices",
            Category::Network => "network",
            Category::Capture => "capture",
            Category::Exec => "exec",
            Category::Power => "power",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::Files => "read, add, change, and delete your files",
            Category::Processes => "start and stop programs",
            Category::Services => "start and stop Windows services",
            Category::Devices => "reprogram attached devices such as a Pico board",
            Category::Network => {
                "open connections to this PC and to devices on your local network such as your router"
            }
            Category::Capture => "capture your screen and set your clipboard",
            Category::Exec => "run PowerShell commands",
            Category::Power => "restart or shut down this computer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    NotRequired,
    PromptAllow,
    PromptAllowOnce,
    PromptAllowFor(u32),
    PromptDeny,
    GrantCached,
    GrantExpired,
}

impl Decision {
    pub fn as_str(self) -> Cow<'static, str> {
        match self {
            Decision::NotRequired => "not_required".into(),
            Decision::PromptAllow => "prompt_allow".into(),
            Decision::PromptAllowOnce => "prompt_allow_once".into(),
            Decision::PromptAllowFor(minutes) => format!("prompt_allow_{minutes}m").into(),
            Decision::PromptDeny => "prompt_deny".into(),
            Decision::GrantCached => "grant_cached".into(),
            Decision::GrantExpired => "grant_expired".into(),
        }
    }
}

pub const CONSENT_ARG: &str = "consent_minutes";
pub const MAX_CONSENT_MINUTES: u32 = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ask {
    #[default]
    Default,
    Once,
    Minutes(u32),
}

pub fn take_ask(kind: &str, args: &mut Map<String, Value>) -> Result<Ask, String> {
    let ask = match args.remove(CONSENT_ARG) {
        None | Some(Value::Null) => return Ok(Ask::Default),
        Some(v) => v.as_u64(),
    };
    match ask {
        Some(0) => Ok(Ask::Once),
        Some(n) if n <= u64::from(MAX_CONSENT_MINUTES) => Ok(Ask::Minutes(n as u32)),
        _ => Err(format!(
            "{kind}: '{CONSENT_ARG}' must be a whole number from 0 to {MAX_CONSENT_MINUTES} (0 means just this once)"
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Term {
    Once,
    Minutes(u32),
    Session,
}

impl Term {
    pub fn length(self) -> Option<Duration> {
        match self {
            Term::Minutes(m) => Some(Duration::from_secs(u64::from(m) * 60)),
            _ => None,
        }
    }

    pub fn text(self) -> String {
        match self {
            Term::Once => "just this once".to_string(),
            Term::Minutes(m) => format!("for the next {}", minutes_text(m, true)),
            Term::Session => "until this session ends".to_string(),
        }
    }

    pub fn button(self) -> Option<String> {
        match self {
            Term::Once => None,
            Term::Minutes(m) => Some(format!("Allow for {}", minutes_text(m, false))),
            Term::Session => Some("Allow until this session ends".to_string()),
        }
    }
}

fn minutes_text(m: u32, after_next: bool) -> String {
    match (m, m % 60) {
        (1, _) if after_next => "minute".to_string(),
        (1, _) => "1 minute".to_string(),
        (60, _) if after_next => "hour".to_string(),
        (60, _) => "1 hour".to_string(),
        (m, 0) => format!("{} hours", m / 60),
        (m, _) => format!("{m} minutes"),
    }
}

pub fn term_for(category: Category, ask: Ask) -> Term {
    if category == Category::Power {
        return Term::Once;
    }
    match ask {
        Ask::Once => Term::Once,
        Ask::Minutes(m) => Term::Minutes(m),
        Ask::Default => match grant_ttl(category) {
            Some(ttl) if ttl.is_zero() => Term::Once,
            Some(ttl) => Term::Minutes((ttl.as_secs() / 60) as u32),
            None => Term::Session,
        },
    }
}

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

fn tell(request: &Request, step: Step) {
    let observer = OBSERVER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(observer) = observer {
        observer(&Event {
            request: request.clone(),
            step,
        });
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Grant {
    pub category: Category,
    pub granted_at: Instant,
    pub expires_at: Option<Instant>,
}

impl Grant {
    pub fn expires(&self) -> bool {
        self.expires_at.is_some()
    }
}

pub const GATED_KINDS: [(&str, Category); 18] = [
    ("fs.upload", Category::Files),
    ("fs.mkdir", Category::Files),
    ("fs.delete", Category::Files),
    ("fs.write", Category::Files),
    ("pico.save", Category::Files),
    ("proc.kill", Category::Processes),
    ("proc.start", Category::Processes),
    ("svc.control", Category::Services),
    ("pico.bootsel", Category::Devices),
    ("pico.flash", Category::Devices),
    ("pico.reset", Category::Devices),
    ("tunnel.open", Category::Network),
    ("net.repair", Category::Network),
    ("sys.screenshot", Category::Capture),
    ("clip.set", Category::Capture),
    ("ps.exec", Category::Exec),
    ("sys.restart", Category::Power),
    ("sys.shutdown", Category::Power),
];

pub fn category_for(kind: &str) -> Option<Category> {
    GATED_KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, c)| *c)
}

#[derive(Default)]
struct State {
    grants: BTreeMap<Category, Grant>,
    denied: BTreeMap<Category, Instant>,
    asking: BTreeSet<Category>,
    deny_all: bool,
    offset: Duration,
}

impl State {
    fn now(&self) -> Instant {
        Instant::now() + self.offset
    }
}

pub struct Ledger {
    prompt: Prompt,
    timeout: Duration,
    state: Mutex<State>,
    notify: Notify,
}

struct AskGuard<'a> {
    ledger: &'a Ledger,
    category: Category,
    armed: bool,
}

impl Drop for AskGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Ok(mut st) = self.ledger.state.lock() {
                st.asking.remove(&self.category);
            }
            self.ledger.notify.notify_waiters();
        }
    }
}

type Gate = (Decision, Result<(), ConsentError>);

impl Ledger {
    pub fn new(prompt: Prompt) -> Ledger {
        Ledger::with_timeout(prompt, PROMPT_TIMEOUT)
    }

    pub fn with_timeout(prompt: Prompt, timeout: Duration) -> Ledger {
        Ledger {
            prompt,
            timeout,
            state: Mutex::new(State::default()),
            notify: Notify::new(),
        }
    }

    pub fn prompt_timeout(&self) -> Duration {
        self.timeout
    }

    async fn ask(&self, cancel: &CancellationToken, req: &Request) -> Result<Answer, ConsentError> {
        let ask_cancel = cancel.child_token();
        let prompt = (self.prompt)(ask_cancel.clone(), req.clone());
        tokio::pin!(prompt);
        tokio::select! {
            answer = &mut prompt => answer,
            _ = tokio::time::sleep(self.timeout) => {
                ask_cancel.cancel();
                let _ = tokio::time::timeout(PROMPT_CLOSE_WAIT, &mut prompt).await;
                Err(ConsentError::TimedOut)
            }
            _ = cancel.cancelled() => {
                ask_cancel.cancel();
                let _ = tokio::time::timeout(PROMPT_CLOSE_WAIT, &mut prompt).await;
                Err(ConsentError::Cancelled)
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn deny_everything(&self) {
        self.lock().deny_all = true;
    }

    pub fn is_deny_everything(&self) -> bool {
        self.lock().deny_all
    }

    #[cfg(test)]
    pub fn advance(&self, by: Duration) {
        self.lock().offset += by;
    }

    pub async fn require(&self, cancel: &CancellationToken, mut req: Request) -> Gate {
        let category = match req.category {
            Some(c) => c,
            None => match category_for(&req.kind) {
                Some(c) => {
                    req.category = Some(c);
                    c
                }
                None => return (Decision::NotRequired, Ok(())),
            },
        };
        let term = term_for(category, req.ask);
        let needs = match req.ask {
            Ask::Minutes(_) => term.length(),
            _ => None,
        };

        loop {
            if cancel.is_cancelled() {
                return (Decision::PromptDeny, Err(ConsentError::Cancelled));
            }

            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let ask = {
                let mut st = self.lock();
                if st.deny_all {
                    return (Decision::PromptDeny, Err(ConsentError::Denied));
                }
                let now = st.now();
                if let Some(g) = st.grants.get(&category).copied() {
                    match g.expires_at {
                        Some(exp) if now >= exp => {
                            st.grants.remove(&category);
                        }
                        Some(exp) if needs.is_some_and(|d| exp - now < d) => {}
                        _ => return (Decision::GrantCached, Ok(())),
                    }
                }
                if let Some(until) = st.denied.get(&category).copied() {
                    if now < until {
                        return (Decision::PromptDeny, Err(ConsentError::Denied));
                    }
                    st.denied.remove(&category);
                }
                if st.asking.contains(&category) {
                    false
                } else {
                    st.asking.insert(category);
                    true
                }
            };

            if !ask {
                tokio::select! {
                    _ = &mut notified => {}
                    _ = cancel.cancelled() => {
                        return (Decision::PromptDeny, Err(ConsentError::Cancelled));
                    }
                }
                continue;
            }

            let mut guard = AskGuard {
                ledger: self,
                category,
                armed: true,
            };
            tell(&req, Step::Asking);
            let picked = self.ask(cancel, &req).await.map(|answer| match answer {
                Answer::No => None,
                Answer::Once => Some(Term::Once),
                Answer::ForGrant => Some(term),
            });
            tell(
                &req,
                match &picked {
                    Ok(Some(term)) => Step::Allowed(*term),
                    Ok(None) => Step::Denied,
                    Err(ConsentError::TimedOut) => Step::TimedOut,
                    Err(ConsentError::Cancelled) => Step::Withdrawn,
                    Err(ConsentError::Denied) => Step::Denied,
                    Err(ConsentError::Other(msg)) => Step::Failed(msg.clone()),
                },
            );
            {
                let mut st = self.lock();
                st.asking.remove(&category);
                match &picked {
                    Ok(Some(Term::Once)) | Err(_) => {}
                    Ok(Some(term)) => {
                        let grant = new_grant(category, *term, st.now());
                        st.grants.insert(category, grant);
                    }
                    Ok(None) => {
                        let until = st.now() + DENY_BACKOFF;
                        st.denied.insert(category, until);
                    }
                }
            }
            guard.armed = false;
            self.notify.notify_waiters();

            return match picked {
                Err(e) => (Decision::PromptDeny, Err(e)),
                Ok(None) => (Decision::PromptDeny, Err(ConsentError::Denied)),
                Ok(Some(Term::Once)) => (Decision::PromptAllowOnce, Ok(())),
                Ok(Some(Term::Minutes(m))) => (Decision::PromptAllowFor(m), Ok(())),
                Ok(Some(Term::Session)) => (Decision::PromptAllow, Ok(())),
            };
        }
    }

    pub fn grants(&self) -> Vec<Grant> {
        let st = self.lock();
        let now = st.now();
        let mut out: Vec<Grant> = st
            .grants
            .values()
            .filter(|g| match g.expires_at {
                Some(exp) => now < exp,
                None => true,
            })
            .copied()
            .collect();
        out.sort_by_key(|g| g.category.as_str());
        out
    }

    pub fn revoke_all(&self) -> usize {
        let mut st = self.lock();
        let n = st.grants.len();
        st.grants.clear();
        st.denied.clear();
        n
    }
}

pub fn grant_ttl(category: Category) -> Option<Duration> {
    match category {
        Category::Exec => Some(EXEC_GRANT_TTL),
        Category::Files => Some(FILES_GRANT_TTL),
        Category::Power => Some(Duration::ZERO),
        _ => None,
    }
}

pub fn scope_text(category: Option<Category>) -> String {
    match category {
        Some(c) => term_for(c, Ask::Default).text(),
        None => Term::Session.text(),
    }
}

fn new_grant(category: Category, term: Term, now: Instant) -> Grant {
    Grant {
        category,
        granted_at: now,
        expires_at: term.length().map(|d| now + d),
    }
}

const SUMMARY_CHARS: usize = 2000;
const SUMMARY_LINES: usize = 40;
const DETAIL_CHARS: usize = 200_000;
const DETAIL_LINES: usize = 20_000;
const CUT_NOTE: &str = "(The rest is too long to show here.)";

pub const PROMPT_TITLE: &str = "Allow this Handoff request?";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptParts {
    pub instruction: String,
    pub summary: String,
    pub scope: String,
    pub caution: String,
    pub command: Option<String>,
    pub detail: Option<String>,
    pub grant_button: Option<String>,
}

pub fn prompt_parts(req: &Request) -> PromptParts {
    let shown = for_display(&req.summary, SUMMARY_CHARS, SUMMARY_LINES);
    let text = shown.text.trim();
    let (head, command) = match text.split_once("\n\n") {
        Some((head, rest)) => (head.trim().to_string(), Some(rest.trim().to_string())),
        None if text.contains('\n') => (String::new(), Some(text.to_string())),
        None => (text.to_string(), None),
    };
    let mut command = command.filter(|c| !c.is_empty());
    if shown.cut {
        match &mut command {
            Some(c) => c.push_str(&format!("\n{CUT_NOTE}")),
            None => command = Some(CUT_NOTE.to_string()),
        }
    }
    let detail = match &req.detail {
        Some(full) => {
            let shown = for_display(full, DETAIL_CHARS, DETAIL_LINES);
            let mut text = shown.text;
            if shown.cut {
                text.push_str(&format!("\n{CUT_NOTE}"));
            }
            Some(text).filter(|t| !t.is_empty())
        }
        None => command.clone(),
    };
    let head = if head.is_empty() && command.is_none() {
        "Run a command that can change this computer.".to_string()
    } else {
        head
    };
    let term = req.term();
    let granted = req
        .category
        .map(Category::label)
        .unwrap_or("run this kind of command");
    let scope = match term {
        Term::Once => "If you allow it, only this request goes ahead. They have to ask again for anything else."
            .to_string(),
        term => format!(
            "If you allow it, they can {granted} {}. For any other kind of change they have to ask you again.",
            term.text()
        ),
    };
    PromptParts {
        instruction: "The person helping you wants to make a change on this computer".to_string(),
        summary: head,
        scope,
        caution: "Only allow it if you know this person and asked them for help.".to_string(),
        command,
        detail,
        grant_button: term.button(),
    }
}

impl PromptParts {
    pub fn scope_line(&self) -> String {
        let mut out = self.scope.clone();
        if self.grant_button.is_some() {
            out.push_str(" Choose Just this once to allow only this request.");
        }
        out
    }
}

pub fn countdown_text(left: Duration) -> String {
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    format!(
        "If you don't answer, Handoff says no for you in {}:{:02}.",
        secs / 60,
        secs % 60
    )
}

pub fn prompt_text(req: &Request) -> String {
    let parts = prompt_parts(req);
    let mut out = format!("{}.\n\n", parts.instruction);
    for piece in [Some(&parts.summary), parts.command.as_ref()]
        .into_iter()
        .flatten()
    {
        if !piece.is_empty() {
            out.push_str(piece);
            out.push_str("\n\n");
        }
    }
    out.push_str(&parts.scope);
    out.push_str("\n\n");
    out.push_str(&parts.caution);
    out.push_str(&format!(
        " If you don't answer within {} minutes, Handoff says no for you.",
        PROMPT_TIMEOUT.as_secs() / 60
    ));
    out
}

pub fn system_prompt() -> Prompt {
    Arc::new(|cancel, req| Box::pin(system_prompt_impl(cancel, req)))
}

#[cfg(windows)]
#[path = "consent_dialog.rs"]
pub mod dialog;

#[cfg(windows)]
async fn system_prompt_impl(
    cancel: CancellationToken,
    req: Request,
) -> Result<Answer, ConsentError> {
    use std::sync::atomic::{AtomicU32, Ordering};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
    }

    if cancel.is_cancelled() {
        return Err(ConsentError::Cancelled);
    }
    let thread = Arc::new(AtomicU32::new(0));
    let seen = thread.clone();
    let shown = req.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        seen.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        dialog::show(&shown, &dialog::Options::default())
    });
    tokio::select! {
        joined = &mut task => {
            joined.map_err(|e| ConsentError::Other(format!("consent prompt failed: {e}")))?
        }
        _ = cancel.cancelled() => {
            let deadline = Instant::now() + PROMPT_CLOSE_WAIT;
            let mut closed = false;
            loop {
                let id = thread.load(Ordering::SeqCst);
                if id != 0 {
                    dialog::close(id);
                }
                match tokio::time::timeout(Duration::from_millis(100), &mut task).await {
                    Ok(_) => {
                        closed = true;
                        break;
                    }
                    Err(_) if Instant::now() >= deadline => break,
                    Err(_) => {}
                }
            }
            crate::slog!("consent prompt withdrawn kind={} closed={closed}", req.kind);
            Err(ConsentError::Cancelled)
        }
    }
}

pub fn typed_question(term: Term) -> String {
    match term {
        Term::Once => {
            "Type y and press Enter to allow it. Press Enter on its own to say no: ".to_string()
        }
        term => format!(
            "Type y and press Enter to allow it {}, or o to allow just this once. Press Enter on its own to say no: ",
            term.text()
        ),
    }
}

pub fn typed_answer(line: &str, term: Term) -> Answer {
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" if term == Term::Once => Answer::Once,
        "y" | "yes" => Answer::ForGrant,
        "o" | "once" => Answer::Once,
        _ => Answer::No,
    }
}

#[cfg(not(windows))]
async fn system_prompt_impl(
    cancel: CancellationToken,
    req: Request,
) -> Result<Answer, ConsentError> {
    if cancel.is_cancelled() {
        return Err(ConsentError::Cancelled);
    }
    let term = req.term();
    eprintln!("{}", prompt_text(&req));
    eprint!("{}", typed_question(term));
    let read = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => Err(ConsentError::Other("no answer could be read".into())),
            Ok(_) => Ok(typed_answer(&line, term)),
            Err(e) => Err(ConsentError::Other(e.to_string())),
        }
    });
    tokio::select! {
        joined = read => joined.map_err(|e| ConsentError::Other(e.to_string()))?,
        _ = cancel.cancelled() => Err(ConsentError::Cancelled),
    }
}

#[cfg(all(test, windows))]
#[path = "consent_preview.rs"]
mod preview;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counting(answer: Answer, delay_ms: u64) -> (Prompt, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let prompt: Prompt = Arc::new(move |_, _| {
            let c = c.clone();
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
            let l = l.clone();
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
            let l = l.clone();
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
        let l2 = l.clone();
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
            assert!(!category.as_str().is_empty());
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
        let c = closed.clone();
        let prompt: Prompt = Arc::new(move |cancel: CancellationToken, _| {
            let c = c.clone();
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
        let l2 = l.clone();
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
        let s2 = seen.clone();
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
        v.as_object().cloned().unwrap()
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
        let t2 = terms.clone();
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
        let s2 = seen.clone();
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

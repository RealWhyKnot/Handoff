use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
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
                "open connections to this PC and to devices on your local network, for example your router"
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
    PromptDeny,
    GrantCached,
    GrantExpired,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::NotRequired => "not_required",
            Decision::PromptAllow => "prompt_allow",
            Decision::PromptDeny => "prompt_deny",
            Decision::GrantCached => "grant_cached",
            Decision::GrantExpired => "grant_expired",
        }
    }
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
}

impl Request {
    pub fn for_kind(kind: &str, summary: &str) -> Request {
        Request {
            kind: kind.to_string(),
            category: None,
            summary: summary.to_string(),
        }
    }
}

pub type Prompt = Arc<
    dyn Fn(CancellationToken, Request) -> BoxFuture<'static, Result<bool, ConsentError>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Asking,
    Allowed,
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

    async fn ask(&self, cancel: &CancellationToken, req: &Request) -> Result<bool, ConsentError> {
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
            let answer = self.ask(cancel, &req).await;
            tell(
                &req,
                match &answer {
                    Ok(true) => Step::Allowed,
                    Ok(false) => Step::Denied,
                    Err(ConsentError::TimedOut) => Step::TimedOut,
                    Err(ConsentError::Cancelled) => Step::Withdrawn,
                    Err(ConsentError::Denied) => Step::Denied,
                    Err(ConsentError::Other(msg)) => Step::Failed(msg.clone()),
                },
            );
            {
                let mut st = self.lock();
                st.asking.remove(&category);
                if let Ok(allowed) = &answer {
                    if *allowed {
                        let grant = new_grant(category, st.now());
                        st.grants.insert(category, grant);
                    } else {
                        let until = st.now() + DENY_BACKOFF;
                        st.denied.insert(category, until);
                    }
                }
            }
            guard.armed = false;
            self.notify.notify_waiters();

            return match answer {
                Err(e) => (Decision::PromptDeny, Err(e)),
                Ok(true) => (Decision::PromptAllow, Ok(())),
                Ok(false) => (Decision::PromptDeny, Err(ConsentError::Denied)),
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
    match category.and_then(grant_ttl) {
        Some(ttl) if ttl.is_zero() => "this one time".to_string(),
        Some(ttl) => format!("for the next {} minutes", ttl.as_secs() / 60),
        None => "until this session ends".to_string(),
    }
}

fn new_grant(category: Category, now: Instant) -> Grant {
    let expires_at = match category {
        Category::Exec => Some(now + EXEC_GRANT_TTL),
        Category::Files => Some(now + FILES_GRANT_TTL),
        Category::Power => Some(now),
        _ => None,
    };
    Grant {
        category,
        granted_at: now,
        expires_at,
    }
}

const SUMMARY_CHARS: usize = 2000;
const SUMMARY_LINES: usize = 40;

pub fn prompt_text(req: &Request) -> String {
    let shown = for_display(&req.summary, SUMMARY_CHARS, SUMMARY_LINES);
    let mut summary = if shown.text.trim().is_empty() {
        "Run a command that can change this computer.".to_string()
    } else {
        shown.text.trim().to_string()
    };
    if shown.cut {
        summary.push_str("\n(The rest is too long to show here.)");
    }
    let granted = req
        .category
        .map(Category::label)
        .unwrap_or("run this kind of command");
    let scope = scope_text(req.category);
    let minutes = PROMPT_TIMEOUT.as_secs() / 60;

    format!(
        "The person helping you wants to make a change on this computer.\n\n\
         They asked to:\n{summary}\n\n\
         If you choose Yes, they can {granted} {scope}. \
         For any other kind of change they have to ask you again.\n\n\
         Choose Yes only if you know this person and asked them for help. \
         If you don't answer within {minutes} minutes, Handoff answers No for you."
    )
}

pub fn system_prompt() -> Prompt {
    Arc::new(|cancel, req| Box::pin(system_prompt_impl(cancel, req)))
}

#[cfg(windows)]
fn close_prompt_windows(thread_id: u32) {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumThreadWindows, PostMessageW, IDNO, WM_COMMAND,
    };

    unsafe extern "system" fn press_no(hwnd: HWND, _: LPARAM) -> BOOL {
        let _ = unsafe { PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(IDNO.0 as usize), LPARAM(0)) };
        BOOL(1)
    }
    unsafe {
        let _ = EnumThreadWindows(thread_id, Some(press_no), LPARAM(0));
    }
}

#[cfg(windows)]
async fn system_prompt_impl(cancel: CancellationToken, req: Request) -> Result<bool, ConsentError> {
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
    }

    if cancel.is_cancelled() {
        return Err(ConsentError::Cancelled);
    }
    let text = HSTRING::from(prompt_text(&req));
    let title = HSTRING::from("Allow this Handoff request?");
    let thread = Arc::new(AtomicU32::new(0));
    let seen = thread.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        seen.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        let ret = unsafe {
            MessageBoxW(
                None,
                &text,
                &title,
                MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
            )
        };
        if ret.0 == 0 {
            return Err(ConsentError::Other(format!(
                "consent prompt failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(ret == IDYES)
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
                    close_prompt_windows(id);
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

#[cfg(not(windows))]
async fn system_prompt_impl(cancel: CancellationToken, req: Request) -> Result<bool, ConsentError> {
    if cancel.is_cancelled() {
        return Err(ConsentError::Cancelled);
    }
    eprintln!("{}", prompt_text(&req));
    eprint!("Type y and press Enter to allow it. Press Enter on its own to say no: ");
    let read = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => Err(ConsentError::Other("no answer could be read".into())),
            Ok(_) => {
                let line = line.trim().to_lowercase();
                Ok(line == "y" || line == "yes")
            }
            Err(e) => Err(ConsentError::Other(e.to_string())),
        }
    });
    tokio::select! {
        joined = read => joined.map_err(|e| ConsentError::Other(e.to_string()))?,
        _ = cancel.cancelled() => Err(ConsentError::Cancelled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counting(allow: bool, delay_ms: u64) -> (Prompt, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let prompt: Prompt = Arc::new(move |_, _| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                Ok(allow)
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
        let (p, calls) = counting(true, 0);
        let l = Ledger::new(p);
        for kind in ["pico.reset", "ps.exec", "fs.delete"] {
            assert!(l.require(&tok(), req(kind)).await.1.is_ok(), "{kind}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn second_command_in_the_same_category_reuses_the_grant() {
        let (p, calls) = counting(true, 0);
        let l = Ledger::new(p);
        for kind in ["fs.delete", "fs.mkdir", "fs.upload"] {
            assert!(l.require(&tok(), req(kind)).await.1.is_ok(), "{kind}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_requests_in_one_category_prompt_once() {
        let (p, calls) = counting(true, 25);
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
        let (p, calls) = counting(true, 10);
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
        let (p, calls) = counting(false, 0);
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
        let (p, calls) = counting(true, 0);
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
        let (p, calls) = counting(true, 0);
        let l = Ledger::new(p);
        for _ in 0..3 {
            assert!(l.require(&tok(), req("sys.restart")).await.1.is_ok());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn revoke_all_forces_a_fresh_prompt() {
        let (p, calls) = counting(true, 0);
        let l = Ledger::new(p);
        assert!(l.require(&tok(), req("fs.delete")).await.1.is_ok());
        assert_eq!(l.revoke_all(), 1);
        assert!(l.grants().is_empty());
        assert!(l.require(&tok(), req("fs.delete")).await.1.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn deny_everything_refuses_without_prompting() {
        let (p, calls) = counting(true, 0);
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
        let (p, calls) = counting(true, 0);
        let l = Ledger::new(p);
        let (decision, res) = l.require(&tok(), req("sys.info")).await;
        assert!(res.is_ok());
        assert_eq!(decision, Decision::NotRequired);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn decisions_are_reported_for_the_audit_log() {
        let (p, _) = counting(true, 0);
        let l = Ledger::new(p);
        assert_eq!(
            l.require(&tok(), req("fs.delete")).await.0,
            Decision::PromptAllow
        );
        assert_eq!(
            l.require(&tok(), req("fs.mkdir")).await.0,
            Decision::GrantCached
        );
    }

    #[tokio::test]
    async fn dropped_prompt_releases_the_category() {
        let (slow, _) = counting(true, 5_000);
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
        });
        assert!(text.contains("Stop exe.ssecorp spooler"), "{text}");
        for c in ['\u{202E}', '\u{202C}', '\u{200B}', '\u{7}'] {
            assert!(!text.contains(c));
        }

        let text = prompt_text(&Request {
            kind: "svc.control".into(),
            category: Some(Category::Services),
            summary: "\u{200B}\u{202E}".into(),
        });
        assert!(text.contains("Run a command that can change this computer."));

        let text = prompt_text(&Request {
            kind: "svc.control".into(),
            category: Some(Category::Services),
            summary: "x\n".repeat(80),
        });
        assert!(text.contains("(The rest is too long to show here.)"));
    }

    #[test]
    fn prompt_text_names_the_scope_and_what_is_granted() {
        let text = prompt_text(&Request {
            kind: "ps.exec".into(),
            category: Some(Category::Exec),
            summary: "Run: Get-Date".into(),
        });
        assert!(text.contains("Run: Get-Date"));
        assert!(text.contains("for the next 15 minutes"));
        assert!(text.contains("run PowerShell commands"));

        let text = prompt_text(&Request {
            kind: "sys.restart".into(),
            category: Some(Category::Power),
            summary: String::new(),
        });
        assert!(text.contains("this one time"));
        assert!(text.contains("Run a command that can change this computer."));
        assert!(text.contains("within 2 minutes"));

        let text = prompt_text(&Request {
            kind: "fs.delete".into(),
            category: Some(Category::Files),
            summary: "Delete C:\\x".into(),
        });
        assert!(text.contains("read, add, change, and delete your files for the next 15 minutes"));

        let text = prompt_text(&Request {
            kind: "tunnel.open".into(),
            category: Some(Category::Network),
            summary: String::new(),
        });
        assert!(text.contains("until this session ends"));
    }

    #[tokio::test]
    async fn files_grant_expires_after_fifteen_minutes() {
        let (p, calls) = counting(true, 0);
        let l = Ledger::new(p);
        assert_eq!(
            l.require(&tok(), req("fs.delete")).await.0,
            Decision::PromptAllow
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
            Decision::PromptAllow
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
        let (slow, _) = counting(true, 60_000);
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
        let (p, _) = counting(false, 0);
        let l = Ledger::new(p);
        let _ = l
            .require(&tok(), Request::for_kind("proc.kill", "observer-test"))
            .await;
        set_observer(None);
        assert_eq!(*seen.lock().unwrap(), vec![Step::Asking, Step::Denied]);
    }

    async fn within<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("timed out")
    }
}

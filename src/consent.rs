use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const EXEC_GRANT_TTL: Duration = Duration::from_secs(15 * 60);
pub const DENY_BACKOFF: Duration = Duration::from_secs(60);

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
    Other(String),
}

impl fmt::Display for ConsentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConsentError::Denied => f.write_str("the host declined this request"),
            ConsentError::Cancelled => f.write_str("context canceled"),
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
        Ledger {
            prompt,
            state: Mutex::new(State::default()),
            notify: Notify::new(),
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
            let answer = (self.prompt)(cancel.clone(), req.clone()).await;
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

fn new_grant(category: Category, now: Instant) -> Grant {
    let expires_at = match category {
        Category::Exec => Some(now + EXEC_GRANT_TTL),
        Category::Power => Some(now),
        _ => None,
    };
    Grant {
        category,
        granted_at: now,
        expires_at,
    }
}

pub fn prompt_text(req: &Request) -> String {
    let summary = req.summary.trim();
    let summary = if summary.is_empty() {
        "Run a command that can change this computer."
    } else {
        summary
    };
    let granted = req
        .category
        .map(Category::label)
        .unwrap_or("run this kind of command");

    let scope = match req.category {
        Some(Category::Exec) => format!("for the next {} minutes", EXEC_GRANT_TTL.as_secs() / 60),
        Some(Category::Power) => "this one time".to_string(),
        _ => "for the rest of this session".to_string(),
    };

    format!(
        "Your helper is asking for permission to do something on this computer.\n\n\
         What they asked for now:\n{summary}\n\n\
         Choosing Yes lets the person with your view link {scope}:\n  - {granted}\n\n\
         This does not give them anything else. They will have to ask again for a different kind of change.\n\n\
         Only choose Yes if you trust this person and asked them for help."
    )
}

pub fn system_prompt() -> Prompt {
    Arc::new(|cancel, req| Box::pin(system_prompt_impl(cancel, req)))
}

#[cfg(windows)]
async fn system_prompt_impl(cancel: CancellationToken, req: Request) -> Result<bool, ConsentError> {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };

    if cancel.is_cancelled() {
        return Err(ConsentError::Cancelled);
    }
    let text = HSTRING::from(prompt_text(&req));
    let title = HSTRING::from("Allow this Handoff request?");
    tokio::task::spawn_blocking(move || {
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
    })
    .await
    .map_err(|e| ConsentError::Other(format!("consent prompt failed: {e}")))?
}

#[cfg(not(windows))]
async fn system_prompt_impl(cancel: CancellationToken, req: Request) -> Result<bool, ConsentError> {
    if cancel.is_cancelled() {
        return Err(ConsentError::Cancelled);
    }
    eprintln!("{}", prompt_text(&req));
    eprint!("Allow? [y/N]: ");
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => Err(ConsentError::Other("EOF".into())),
            Ok(_) => {
                let line = line.trim().to_lowercase();
                Ok(line == "y" || line == "yes")
            }
            Err(e) => Err(ConsentError::Other(e.to_string())),
        }
    })
    .await
    .map_err(|e| ConsentError::Other(e.to_string()))?
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
    }
}

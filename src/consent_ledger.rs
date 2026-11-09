use super::consent::Answer;
use super::consent::ConsentError;
use super::consent::DENY_BACKOFF;
use super::consent::PROMPT_CLOSE_WAIT;
use super::consent::PROMPT_TIMEOUT;
use super::consent::Prompt;
use super::consent::Request;
use super::consent::Step;
use super::consent::tell;
use super::consent_policy::Ask;
use super::consent_policy::Category;
use super::consent_policy::Decision;
use super::consent_policy::Term;
use super::consent_policy::category_for;
use super::consent_policy::term_for;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

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

#[derive(Default)]
pub(super) struct State {
    pub(super) grants: BTreeMap<Category, Grant>,
    pub(super) denied: BTreeMap<Category, Instant>,
    pub(super) asking: BTreeSet<Category>,
    pub(super) deny_all: bool,
    pub(super) offset: Duration,
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

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, State> {
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
        drop(st);
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

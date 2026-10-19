use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use tokio_util::sync::CancellationToken;

use crate::consent::{system_prompt, Decision, Ledger, Request};
use crate::dispatch::HandlerError;

static SESSION_LEDGER: LazyLock<Mutex<Arc<Ledger>>> =
    LazyLock::new(|| Mutex::new(Arc::new(Ledger::new(system_prompt()))));
static DENY_ALL_RISKY: AtomicBool = AtomicBool::new(false);
static LAST_DECISION: LazyLock<Mutex<BTreeMap<String, Decision>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn session() -> std::sync::MutexGuard<'static, Arc<Ledger>> {
    SESSION_LEDGER.lock().unwrap_or_else(|e| e.into_inner())
}

fn decisions() -> std::sync::MutexGuard<'static, BTreeMap<String, Decision>> {
    LAST_DECISION.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn set_session_ledger(ledger: Arc<Ledger>) -> Arc<Ledger> {
    std::mem::replace(&mut *session(), ledger)
}

pub fn reset_risk_consent() {
    let ledger = Arc::new(Ledger::new(system_prompt()));
    if DENY_ALL_RISKY.load(Ordering::SeqCst) {
        ledger.deny_everything();
    }
    *session() = ledger;
    decisions().clear();
}

pub fn ledger() -> Arc<Ledger> {
    session().clone()
}

pub fn deny_all_risky() {
    DENY_ALL_RISKY.store(true, Ordering::SeqCst);
    ledger().deny_everything();
}

pub fn last_consent_decision(kind: &str) -> Decision {
    decisions()
        .get(kind)
        .copied()
        .unwrap_or(Decision::NotRequired)
}

pub async fn require_risk_consent(
    cancel: &CancellationToken,
    kind: &str,
    summary: &str,
) -> Result<(), HandlerError> {
    let (decision, result) = ledger()
        .require(cancel, Request::for_kind(kind, summary))
        .await;
    decisions().insert(kind.to_string(), decision);
    result.map_err(|e| HandlerError::plain(e.to_string()))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::consent::Prompt;

    static SERIAL: Mutex<()> = Mutex::new(());

    pub struct SessionGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl SessionGuard {
        pub fn new() -> SessionGuard {
            let lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            DENY_ALL_RISKY.store(false, Ordering::SeqCst);
            reset_risk_consent();
            SessionGuard { _lock: lock }
        }

        pub fn with_prompt(prompt: Prompt) -> SessionGuard {
            let guard = SessionGuard::new();
            set_session_ledger(Arc::new(Ledger::new(prompt)));
            guard
        }
    }

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            DENY_ALL_RISKY.store(false, Ordering::SeqCst);
            reset_risk_consent();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::SessionGuard;
    use super::*;
    use crate::consent::Prompt;
    use std::sync::atomic::AtomicUsize;

    fn allow_all() -> (Prompt, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let prompt: Prompt = Arc::new(move |_, _| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            })
        });
        (prompt, calls)
    }

    #[tokio::test]
    async fn consent_is_asked_and_the_decision_is_recorded() {
        let (prompt, calls) = allow_all();
        let _guard = SessionGuard::with_prompt(prompt);
        let cancel = CancellationToken::new();
        require_risk_consent(&cancel, "fs.write", "Writes a file.")
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(last_consent_decision("fs.write"), Decision::PromptAllow);
        require_risk_consent(&cancel, "fs.write", "Writes a file.")
            .await
            .unwrap();
        assert_eq!(last_consent_decision("fs.write"), Decision::GrantCached);
        assert_eq!(last_consent_decision("sys.info"), Decision::NotRequired);
    }

    #[tokio::test]
    async fn read_only_session_survives_capability_registration() {
        let (prompt, calls) = allow_all();
        let _guard = SessionGuard::with_prompt(prompt);
        deny_all_risky();
        super::super::register_all(&mut crate::dispatch::Router::new(), None);

        let cancel = CancellationToken::new();
        let err = require_risk_consent(&cancel, "fs.write", "Writes a file.")
            .await
            .unwrap_err();
        assert_eq!(err.message, "the host declined this request");
        let err = require_risk_consent(&cancel, "sys.screenshot", "Takes a picture.")
            .await
            .unwrap_err();
        assert_eq!(err.message, "the host declined this request");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(last_consent_decision("fs.write"), Decision::PromptDeny);
    }
}

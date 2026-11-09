use super::new::RECONNECT_TIMEOUT;
use super::new::RECONNECTED;
use super::new::STILL_TRYING;
use super::new::STILL_TRYING_AFTER;
use crate::console::{self, Tone, paint};
use crate::relay::Bridge;
use crate::util;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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

pub(super) async fn reconnect_bridge(
    cancel: &CancellationToken,
    sid: &str,
    bridge: &Bridge,
    backoff: &mut ReconnectBackoff,
) -> bool {
    let mut failures = 0usize;
    loop {
        let delay = backoff.next_delay();
        if !sleep_cancel(cancel, delay).await {
            tracing::info!("reconnect cancelled sid={sid}");
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
                tracing::info!("bridge reconnected sid={sid}");
                console::line(&paint(Tone::Green, RECONNECTED));
                return true;
            }
            Err(err) => {
                if cancel.is_cancelled() {
                    tracing::info!("reconnect ended sid={sid} err=stopped");
                    return false;
                }
                tracing::info!(
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

use super::relay::HEARTBEAT_INTERVAL;
use super::relay::PENDING_RESULTS_CAP;
use super::relay::UNREADABLE_FRAME;
use super::relay::UNREADABLE_NO_KIND;
use super::relay_conn::Conn;
use super::relay_conn::close_conn;
use super::relay_conn::dial_conn;
use super::relay_proto::Command;
use super::relay_proto::Delivery;
use super::relay_proto::Hello;
use super::relay_proto::Incoming;
use super::relay_proto::KeepaliveTiming;
use super::relay_proto::RecvError;
use super::relay_proto::frame_text;
use super::relay_proto::result_payload;
use super::relay_proto::salvage_id;
use crate::dispatch::{Outcome, Spec};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub(super) struct Inner {
    base_url: String,
    write_token: String,
    timing: KeepaliveTiming,
    conn: Mutex<Option<Arc<Conn>>>,
    hello: Mutex<Option<Hello>>,
    presence: AtomicBool,
    pub(super) pending: tokio::sync::Mutex<VecDeque<String>>,
}

impl Inner {
    pub(super) fn new(
        base_url: &str,
        write_token: &str,
        timing: KeepaliveTiming,
        conn: Option<Arc<Conn>>,
    ) -> Inner {
        Inner {
            base_url: base_url.to_string(),
            write_token: write_token.to_string(),
            timing,
            conn: Mutex::new(conn),
            hello: Mutex::new(None),
            presence: AtomicBool::new(false),
            pending: tokio::sync::Mutex::new(VecDeque::new()),
        }
    }
}

#[derive(Clone)]
pub struct Bridge {
    pub(super) inner: Arc<Inner>,
}

impl Bridge {
    pub async fn dial(
        base_url: &str,
        write_token: &str,
        cancel: &CancellationToken,
    ) -> Result<Bridge, String> {
        Bridge::dial_with(base_url, write_token, cancel, KeepaliveTiming::default()).await
    }

    pub async fn dial_with(
        base_url: &str,
        write_token: &str,
        cancel: &CancellationToken,
        timing: KeepaliveTiming,
    ) -> Result<Bridge, String> {
        let conn = dial_conn(base_url, write_token, cancel, timing).await?;
        Ok(Bridge {
            inner: Arc::new(Inner::new(base_url, write_token, timing, Some(conn))),
        })
    }

    pub(super) fn current_conn(&self) -> Result<Arc<Conn>, RecvError> {
        self.inner
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(RecvError::BridgeClosed)
    }

    pub async fn send_hello(
        &self,
        hostname: &str,
        version: &str,
        capabilities: &[String],
        specs: &[Spec],
    ) -> Result<(), String> {
        let mut caps = capabilities.to_vec();
        caps.sort();
        let hello = Hello {
            hostname: hostname.to_string(),
            version: version.to_string(),
            capabilities: caps,
            specs: specs.to_vec(),
        };
        *self.inner.hello.lock().unwrap_or_else(|e| e.into_inner()) = Some(hello.clone());
        self.send_hello_payload(Some(hello)).await
    }

    async fn send_hello_payload(&self, hello: Option<Hello>) -> Result<(), String> {
        let Some(hello) = hello else {
            return Ok(());
        };
        let mut payload = Map::new();
        payload.insert("hostname".into(), json!(hello.hostname));
        payload.insert("version".into(), json!(hello.version));
        payload.insert("os".into(), json!("windows"));
        payload.insert("protocol_version".into(), json!(2));
        payload.insert("capabilities".into(), json!(hello.capabilities));
        if self.inner.presence.load(Ordering::SeqCst) {
            payload.insert("presence".into(), json!(true));
        }
        if !hello.specs.is_empty() {
            payload.insert(
                "command_specs".into(),
                serde_json::to_value(&hello.specs)
                    .map_err(|e| format!("encode the capability list: {e}"))?,
            );
        }
        self.send("hello", Value::Object(payload)).await
    }

    pub fn want_presence(&self) {
        self.inner.presence.store(true, Ordering::SeqCst);
    }

    pub async fn send_command_result(&self, id: &str, out: &Outcome) -> Result<(), String> {
        self.send("command_result", result_payload(id, out)?).await
    }

    pub async fn send_command_result_or_queue(
        &self,
        id: &str,
        out: &Outcome,
    ) -> Result<Delivery, String> {
        let text = frame_text("command_result", result_payload(id, out)?)?;
        let conn = match self.current_conn() {
            Ok(conn) => conn,
            Err(_) => {
                self.queue_result(text).await;
                return Ok(Delivery::Queued);
            }
        };
        let sent = conn.sink.lock().await.send_text(text.clone()).await;
        if sent.is_ok() {
            return Ok(Delivery::Sent);
        }
        self.queue_result(text).await;
        let moved = self
            .current_conn()
            .map(|now| !Arc::ptr_eq(&now, &conn))
            .unwrap_or(false);
        if moved {
            self.flush_pending().await;
            if self.inner.pending.lock().await.is_empty() {
                return Ok(Delivery::Sent);
            }
        }
        Ok(Delivery::Queued)
    }

    async fn queue_result(&self, text: String) {
        let mut pending = self.inner.pending.lock().await;
        while pending.len() >= PENDING_RESULTS_CAP {
            pending.pop_front();
        }
        pending.push_back(text);
    }

    pub async fn pending_results(&self) -> usize {
        self.inner.pending.lock().await.len()
    }

    async fn flush_pending(&self) -> usize {
        let mut pending = self.inner.pending.lock().await;
        let Ok(conn) = self.current_conn() else {
            return 0;
        };
        let mut sent = 0;
        while let Some(text) = pending.pop_front() {
            if conn
                .sink
                .lock()
                .await
                .send_text(text.clone())
                .await
                .is_err()
            {
                pending.push_front(text);
                drop(pending);
                break;
            }
            sent += 1;
        }
        sent
    }

    async fn reply_unreadable(&self, id: &str, why: &str) {
        let out = Outcome {
            ok: false,
            error: why.to_string(),
            ..Outcome::default()
        };
        if let Err(err) = self.send_command_result(id, &out).await {
            tracing::info!("unreadable command reply failed id={id}: {err}");
        }
    }

    pub async fn send_command_progress(&self, id: &str, text: &str) -> Result<(), String> {
        self.send("command_progress", json!({"id": id, "text": text}))
            .await
    }

    pub async fn send_tunnel_data(
        &self,
        tunnel_id: &str,
        stream_id: &str,
        data: &[u8],
    ) -> Result<(), String> {
        self.send(
            "tunnel_data",
            json!({
                "tunnel_id": tunnel_id,
                "stream_id": stream_id,
                "data_base64": base64::engine::general_purpose::STANDARD.encode(data),
            }),
        )
        .await
    }

    pub async fn send_tunnel_stream_close(
        &self,
        tunnel_id: &str,
        stream_id: &str,
        reason: &str,
    ) -> Result<(), String> {
        self.send(
            "tunnel_stream_close",
            json!({"tunnel_id": tunnel_id, "stream_id": stream_id, "reason": reason}),
        )
        .await
    }

    pub async fn send_tunnel_close(&self, tunnel_id: &str, reason: &str) -> Result<(), String> {
        self.send(
            "tunnel_close",
            json!({"tunnel_id": tunnel_id, "reason": reason}),
        )
        .await
    }

    pub async fn recv(&self, cancel: &CancellationToken) -> Result<Command, RecvError> {
        loop {
            let conn = self.current_conn()?;
            let mut rx = conn.rx.lock().await;
            let incoming = tokio::select! {
                _ = cancel.cancelled() => return Err(RecvError::Cancelled),
                item = rx.recv() => item,
            };
            drop(rx);
            match incoming {
                None => {
                    if conn.stale.load(Ordering::SeqCst) {
                        return Err(RecvError::Stale);
                    }
                    return Err(RecvError::Eof);
                }
                Some(Incoming::Text(text)) => match Command::parse(&text) {
                    Some(cmd) if cmd.kind.is_empty() && !cmd.id.is_empty() => {
                        tracing::info!("command without a kind id={}", cmd.id);
                        self.reply_unreadable(&cmd.id, UNREADABLE_NO_KIND).await;
                        continue;
                    }
                    Some(cmd) => return Ok(cmd),
                    None => {
                        match salvage_id(&text) {
                            Some(id) => {
                                tracing::info!("unreadable command id={id} len={}", text.len());
                                self.reply_unreadable(&id, UNREADABLE_FRAME).await;
                            }
                            None => {
                                tracing::info!("dropped unreadable frame len={}", text.len())
                            }
                        }
                        continue;
                    }
                },
                Some(Incoming::Closed(frame)) => {
                    if conn.stale.load(Ordering::SeqCst) {
                        return Err(RecvError::Stale);
                    }
                    return Err(match frame {
                        Some((code, reason)) => RecvError::Closed { code, reason },
                        None => RecvError::Eof,
                    });
                }
                Some(Incoming::Failed(msg)) => {
                    if conn.stale.load(Ordering::SeqCst) {
                        return Err(RecvError::Stale);
                    }
                    return Err(RecvError::Io(msg));
                }
            }
        }
    }

    pub async fn reconnect(&self, cancel: &CancellationToken) -> Result<(), String> {
        let conn = dial_conn(
            &self.inner.base_url,
            &self.inner.write_token,
            cancel,
            self.inner.timing,
        )
        .await?;

        let old = self
            .inner
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(conn);
        if let Some(old) = old {
            old.stop_keepalive();
            close_conn(&old, "reconnect").await;
        }

        let hello = self
            .inner
            .hello
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.send_hello_payload(hello)
            .await
            .map_err(|e| format!("replay hello: {e}"))?;
        let flushed = self.flush_pending().await;
        if flushed > 0 {
            tracing::info!("resent {flushed} queued result(s) after reconnect");
        }
        Ok(())
    }

    pub async fn close(&self) {
        let conn = self
            .inner
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(conn) = conn {
            conn.stop_keepalive();
            close_conn(&conn, "bye").await;
        }
    }

    pub fn start_heartbeat(&self, cancel: CancellationToken) {
        let bridge = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval_at(
                tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
                HEARTBEAT_INTERVAL,
            );
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = ticker.tick() => {
                        let _ = bridge.send("ping", Value::Null).await;
                    }
                }
            }
        });
    }

    pub async fn send(&self, kind: &str, payload: Value) -> Result<(), String> {
        let text = frame_text(kind, payload)?;
        let conn = self
            .current_conn()
            .map_err(|e| format!("send a relay frame: {e}"))?;
        let mut writer = conn.sink.lock().await;
        writer.send_text(text).await
    }
}

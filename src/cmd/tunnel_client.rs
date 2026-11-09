use super::tunnel_http::rewrite_http_host_header;
use super::tunnel_session::LoopEnd;
use super::tunnel_session::WsSink;
use super::tunnel_session::WsSource;
use super::tunnel_session::close_text;
use super::tunnel_session::inflate;
use super::tunnel_session::panic_text;
use super::tunnel_wire::WireFrame;
use crate::zwire::{Packer, Unpacker};
use base64::Engine;
use futures_util::{FutureExt, SinkExt, StreamExt};
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

struct StreamEntry {
    writer: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    cancel: CancellationToken,
}

pub(super) struct TunnelClient {
    sink: tokio::sync::Mutex<Option<(WsSink, Option<Packer>)>>,
    http_host: String,
    streams: Mutex<HashMap<String, StreamEntry>>,
}

impl TunnelClient {
    pub(super) fn new(sink: WsSink, zip: Option<Packer>, http_host: String) -> TunnelClient {
        TunnelClient {
            sink: tokio::sync::Mutex::new(Some((sink, zip))),
            http_host,
            streams: Mutex::new(HashMap::new()),
        }
    }

    fn streams(&self) -> std::sync::MutexGuard<'_, HashMap<String, StreamEntry>> {
        self.streams.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) async fn accept_loop(
        self: Arc<Self>,
        session: CancellationToken,
        listener: Arc<tokio::net::TcpListener>,
    ) {
        loop {
            let accepted = tokio::select! {
                _ = session.cancelled() => return,
                r = listener.accept() => r,
            };
            let conn = match accepted {
                Ok((c, _)) => c,
                Err(err) => {
                    if session.is_cancelled() {
                        return;
                    }
                    tracing::info!("tunnel-client accept error: {err}");
                    return;
                }
            };
            let stream_id = new_stream_id();
            let (rd, wr) = conn.into_split();
            let stream_cancel = session.child_token();
            self.streams().insert(
                stream_id.clone(),
                StreamEntry {
                    writer: Arc::new(tokio::sync::Mutex::new(wr)),
                    cancel: stream_cancel.clone(),
                },
            );
            let mut open = WireFrame::kind("stream_open");
            open.stream_id = stream_id.clone();
            if self.send_frame(&open).await.is_err() {
                self.drop_stream(&stream_id, "stream open send failed");
                continue;
            }
            tokio::spawn(Arc::clone(&self).copy_to_relay(stream_id, rd, stream_cancel));
        }
    }

    pub(super) async fn read_loop(
        &self,
        session: &CancellationToken,
        source: &mut WsSource,
        unzip: &mut Option<Unpacker>,
    ) -> LoopEnd {
        loop {
            let next = tokio::select! {
                _ = session.cancelled() => return LoopEnd::Cancelled,
                n = source.next() => n,
            };
            let msg = match next.map(|r| r.map(|m| inflate(m, unzip))) {
                Some(Ok(Ok(m))) => m,
                Some(Ok(Err(err))) => return LoopEnd::Failed(err),
                Some(Err(err)) => return LoopEnd::Failed(err.to_string()),
                None => return LoopEnd::Failed("EOF".to_string()),
            };
            let text = match msg {
                Message::Text(t) => t,
                Message::Close(frame) => {
                    return LoopEnd::Failed(close_text(
                        frame.map(|f| (u16::from(f.code), f.reason.to_string())),
                    ));
                }
                _ => continue,
            };
            let Ok(frame) = serde_json::from_str::<WireFrame>(&text) else {
                continue;
            };
            match frame.kind.as_str() {
                "data" => {
                    let payload = match base64::engine::general_purpose::STANDARD
                        .decode(&frame.data_base64)
                    {
                        Ok(p) => p,
                        Err(err) => {
                            tracing::info!("tunnel-client decode data error: {err}");
                            continue;
                        }
                    };
                    self.write_to_stream(&frame.stream_id, &payload).await;
                }
                "stream_close" => self.drop_stream(&frame.stream_id, &frame.reason),
                "tunnel_closed" => {
                    crate::outln!("relay closed tunnel: {}", frame.reason);
                    return LoopEnd::ClosedByRelay;
                }
                "error" => crate::errln!("relay error: {}", frame.message),
                _ => {}
            }
        }
    }

    async fn copy_to_relay(
        self: Arc<Self>,
        stream_id: String,
        rd: OwnedReadHalf,
        cancel: CancellationToken,
    ) {
        let task = AssertUnwindSafe(self.copy_inner(&stream_id, rd, &cancel)).catch_unwind();
        match task.await {
            Ok(()) => self.drop_stream(&stream_id, "local end of stream"),
            Err(payload) => {
                tracing::info!(
                    "tunnel-client copy panic stream={stream_id}: {}",
                    panic_text(payload.as_ref())
                );
                self.drop_stream(&stream_id, "internal error");
            }
        }
    }

    async fn copy_inner(&self, stream_id: &str, mut rd: OwnedReadHalf, cancel: &CancellationToken) {
        let mut buf = vec![0u8; 16 * 1024];
        let mut rewrite = !self.http_host.is_empty();
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let res = tokio::select! {
                _ = cancel.cancelled() => Err(None),
                r = rd.read(&mut buf) => match r {
                    Ok(0) => Err(None),
                    Ok(n) => Ok(n),
                    Err(e) => Err(Some(e)),
                },
            };
            match res {
                Ok(n) => {
                    let mut chunk = buf[..n].to_vec();
                    if rewrite {
                        pending.extend_from_slice(&chunk);
                        match rewrite_http_host_header(&pending, &self.http_host) {
                            Some(rewritten) => chunk = rewritten,
                            None => continue,
                        }
                        pending.clear();
                        rewrite = false;
                    }
                    let mut frame = WireFrame::kind("data");
                    frame.stream_id = stream_id.to_string();
                    frame.data_base64 = base64::engine::general_purpose::STANDARD.encode(&chunk);
                    if self.send_frame(&frame).await.is_err() {
                        return;
                    }
                }
                Err(err) => {
                    if let Some(err) = err {
                        tracing::info!("tunnel-client local read error stream={stream_id}: {err}");
                    }
                    if rewrite && !pending.is_empty() {
                        let mut frame = WireFrame::kind("data");
                        frame.stream_id = stream_id.to_string();
                        frame.data_base64 =
                            base64::engine::general_purpose::STANDARD.encode(&pending);
                        let _ = self.send_frame(&frame).await;
                    }
                    let mut close = WireFrame::kind("stream_close");
                    close.stream_id = stream_id.to_string();
                    close.reason = "eof".to_string();
                    let _ = self.send_frame(&close).await;
                    return;
                }
            }
        }
    }

    async fn send_frame(&self, frame: &WireFrame) -> Result<(), String> {
        let data =
            serde_json::to_string(frame).map_err(|e| format!("encode a tunnel frame: {e}"))?;
        let mut sink = self.sink.lock().await;
        match sink.as_mut() {
            Some((s, Some(z))) => {
                let packed = z
                    .pack(data.as_bytes())
                    .map_err(|e| format!("compress a tunnel frame: {e}"))?;
                s.send(Message::binary(packed))
                    .await
                    .map_err(|e| format!("send a tunnel frame: {e}"))
            }
            Some((s, None)) => s
                .send(Message::text(data))
                .await
                .map_err(|e| format!("send a tunnel frame: {e}")),
            None => Err("tunnel client closed".to_string()),
        }
    }

    async fn write_to_stream(&self, stream_id: &str, payload: &[u8]) {
        let writer = match self.streams().get(stream_id) {
            Some(e) => Arc::clone(&e.writer),
            None => return,
        };
        let result = writer.lock().await.write_all(payload).await;
        if let Err(err) = result {
            self.drop_stream(stream_id, &format!("write error: {err}"));
        }
    }

    fn drop_stream(&self, stream_id: &str, reason: &str) {
        let removed = self.streams().remove(stream_id);
        if let Some(entry) = removed {
            entry.cancel.cancel();
            drop(entry);
            tracing::info!("tunnel-client stream end stream={stream_id} reason={reason}");
        }
    }

    pub(super) async fn shutdown(&self, reason: &str) {
        self.sink.lock().await.take();
        let entries: Vec<StreamEntry> = self.streams().drain().map(|(_, e)| e).collect();
        for e in entries {
            e.cancel.cancel();
        }
        let _ = reason;
    }
}

pub fn new_stream_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u128(nanos);
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("s_{:016x}", h.finish())
}

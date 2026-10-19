use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use crate::dispatch::{Clamp, Outcome, Spec};
use crate::util;
use crate::zwire::{self, Packer, Unpacker};

pub const READ_LIMIT: usize = 8 << 20;
pub const PING_INTERVAL: Duration = Duration::from_secs(15);
pub const PING_TIMEOUT: Duration = Duration::from_secs(30);
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
pub const PENDING_RESULTS_CAP: usize = 32;
pub const PRESENCE_KIND: &str = "control.presence";
pub const UNREADABLE_FRAME: &str = "This computer couldn't read that request, so it didn't run it.";
pub const UNREADABLE_NO_KIND: &str = "The request didn't say which command to run, so nothing ran.";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;
type WsSource = SplitStream<WsStream>;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MintResponse {
    #[serde(default)]
    pub write_token: String,
    #[serde(default)]
    pub view_token: String,
    #[serde(default)]
    pub view_url: String,
    #[serde(default)]
    pub created_ts: i64,
}

pub async fn mint(base_url: &str) -> Result<MintResponse, String> {
    util::install_crypto_provider();
    let url = format!("{}/api/sessions", base_url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("Post {url:?}: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.as_u16() != 200 {
        return Err(format!("mint: {} {}", status.as_u16(), body.trim()));
    }
    serde_json::from_str(&body).map_err(|e| format!("mint: decode: {e}"))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Command {
    pub id: String,
    pub kind: String,
    pub timeout_ms: i64,
    pub extras: Map<String, Value>,
}

impl Command {
    pub fn parse(data: &str) -> Option<Command> {
        let Ok(Value::Object(mut raw)) = serde_json::from_str::<Value>(data) else {
            return None;
        };
        let mut cmd = Command::default();
        if let Some(Value::String(s)) = raw.remove("id") {
            cmd.id = s;
        }
        if let Some(Value::String(s)) = raw.remove("kind") {
            cmd.kind = s;
        }
        if let Some(v) = raw.remove("timeout_ms") {
            if let Some(n) = v.as_i64() {
                cmd.timeout_ms = n;
            }
        }
        cmd.extras = raw;
        Some(cmd)
    }
}

#[derive(Serialize)]
struct TelemetryEvent<'a> {
    ts: i64,
    kind: &'a str,
    payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecvError {
    Cancelled,
    Stale,
    BridgeClosed,
    Closed { code: u16, reason: String },
    Eof,
    Io(String),
}

impl RecvError {
    pub fn is_eof(&self) -> bool {
        matches!(self, RecvError::Eof | RecvError::Closed { .. })
    }
}

fn close_code_name(code: u16) -> String {
    let name = match code {
        1000 => "StatusNormalClosure",
        1001 => "StatusGoingAway",
        1002 => "StatusProtocolError",
        1003 => "StatusUnsupportedData",
        1005 => "StatusNoStatusRcvd",
        1006 => "StatusAbnormalClosure",
        1007 => "StatusInvalidFramePayloadData",
        1008 => "StatusPolicyViolation",
        1009 => "StatusMessageTooBig",
        1010 => "StatusMandatoryExtension",
        1011 => "StatusInternalError",
        1012 => "StatusServiceRestart",
        1013 => "StatusTryAgainLater",
        1014 => "StatusBadGateway",
        1015 => "StatusTLSHandshake",
        other => return format!("StatusCode({other})"),
    };
    name.to_string()
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvError::Cancelled => f.write_str("stopped"),
            RecvError::Stale => f.write_str("bridge stale: ping timeout"),
            RecvError::BridgeClosed => f.write_str("bridge is closed"),
            RecvError::Closed { code, reason } => write!(
                f,
                "ws closed: code={} ({}) reason={:?}: EOF",
                code,
                close_code_name(*code),
                reason
            ),
            RecvError::Eof => f.write_str("EOF"),
            RecvError::Io(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for RecvError {}

#[derive(Debug, Clone, Copy)]
pub struct KeepaliveTiming {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for KeepaliveTiming {
    fn default() -> Self {
        KeepaliveTiming {
            interval: PING_INTERVAL,
            timeout: PING_TIMEOUT,
        }
    }
}

enum Incoming {
    Text(String),
    Closed(Option<(u16, String)>),
    Failed(String),
}

struct Writer {
    sink: WsSink,
    zip: Option<Packer>,
}

impl Writer {
    async fn send_text(&mut self, text: String) -> Result<(), String> {
        let msg = match &mut self.zip {
            Some(zip) => Message::binary(zip.pack(text.as_bytes()).map_err(|e| e.to_string())?),
            None => Message::text(text),
        };
        self.sink.send(msg).await.map_err(|e| e.to_string())
    }
}

struct Conn {
    sink: tokio::sync::Mutex<Writer>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Incoming>>,
    stale: AtomicBool,
    reader: AbortHandle,
    keepalive: Mutex<Option<JoinHandle<()>>>,
}

impl Conn {
    fn stop_keepalive(&self) {
        if let Some(h) = self
            .keepalive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            h.abort();
        }
    }
}

#[derive(Clone)]
struct Hello {
    hostname: String,
    version: String,
    capabilities: Vec<String>,
    specs: Vec<Spec>,
}

struct Inner {
    base_url: String,
    write_token: String,
    timing: KeepaliveTiming,
    conn: Mutex<Option<Arc<Conn>>>,
    hello: Mutex<Option<Hello>>,
    presence: AtomicBool,
    pending: tokio::sync::Mutex<VecDeque<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    Queued,
}

impl Inner {
    fn new(
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
    inner: Arc<Inner>,
}

pub fn http_to_ws(base: &str) -> Result<String, String> {
    let mut u = url::Url::parse(base).map_err(|e| e.to_string())?;
    let scheme = match u.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => return Err(format!("unsupported scheme {other:?} in {base:?}")),
    };
    u.set_scheme(scheme)
        .map_err(|_| format!("unsupported scheme in {base:?}"))?;
    Ok(u.to_string())
}

async fn dial_conn(
    base_url: &str,
    write_token: &str,
    cancel: &CancellationToken,
    timing: KeepaliveTiming,
) -> Result<Arc<Conn>, String> {
    util::install_crypto_provider();
    let ws_url = format!("{}/ws", http_to_ws(base_url)?.trim_end_matches('/'));
    let mut request = ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("ws dial: {e}"))?;
    let token = HeaderValue::from_str(write_token).map_err(|e| format!("ws dial: {e}"))?;
    request.headers_mut().insert("X-Write-Token", token);
    request
        .headers_mut()
        .insert(zwire::HEADER, HeaderValue::from_static(zwire::ZSTD));

    let config = WebSocketConfig::default()
        .max_message_size(Some(READ_LIMIT))
        .max_frame_size(Some(READ_LIMIT));

    let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), false);
    let (stream, response) = tokio::select! {
        _ = cancel.cancelled() => return Err("ws dial: stopped".to_string()),
        res = connect => res.map_err(|e| format!("ws dial: {e}"))?,
    };
    let zipped = zwire::offers_zstd(
        response
            .headers()
            .get(zwire::HEADER)
            .and_then(|v| v.to_str().ok()),
    );
    let (zip, unzip) = if zipped {
        let zip = Packer::new(zwire::LEVEL).map_err(|e| format!("ws dial: {e}"))?;
        let unzip = Unpacker::new(READ_LIMIT).map_err(|e| format!("ws dial: {e}"))?;
        (Some(zip), Some(unzip))
    } else {
        (None, None)
    };

    let (sink, source) = stream.split();
    let (tx, rx) = mpsc::unbounded_channel();
    let (pong_tx, pong_rx) = watch::channel(0u64);
    let reader = tokio::spawn(read_loop(source, tx, pong_tx, unzip));

    let conn = Arc::new(Conn {
        sink: tokio::sync::Mutex::new(Writer { sink, zip }),
        rx: tokio::sync::Mutex::new(rx),
        stale: AtomicBool::new(false),
        reader: reader.abort_handle(),
        keepalive: Mutex::new(None),
    });
    let handle = tokio::spawn(keepalive(conn.clone(), pong_rx, timing));
    *conn.keepalive.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    Ok(conn)
}

async fn read_loop(
    mut source: WsSource,
    tx: mpsc::UnboundedSender<Incoming>,
    pongs: watch::Sender<u64>,
    mut unzip: Option<Unpacker>,
) {
    while let Some(item) = source.next().await {
        match item {
            Ok(Message::Text(text)) => {
                if tx.send(Incoming::Text(text.to_string())).is_err() {
                    return;
                }
            }
            Ok(Message::Binary(bytes)) => {
                let Some(unzip) = unzip.as_mut() else {
                    continue;
                };
                let text = unzip
                    .unpack(&bytes)
                    .map_err(|e| e.to_string())
                    .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()));
                match text {
                    Ok(text) => {
                        if tx.send(Incoming::Text(text)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Incoming::Failed(format!("bad compressed frame: {e}")));
                        return;
                    }
                }
            }
            Ok(Message::Pong(_)) => {
                pongs.send_modify(|n| *n += 1);
            }
            Ok(Message::Close(frame)) => {
                let info = frame.map(|f| (u16::from(f.code), f.reason.to_string()));
                let _ = tx.send(Incoming::Closed(info));
                return;
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Incoming::Failed(e.to_string()));
                return;
            }
        }
    }
    let _ = tx.send(Incoming::Closed(None));
}

async fn keepalive(conn: Arc<Conn>, mut pongs: watch::Receiver<u64>, timing: KeepaliveTiming) {
    loop {
        tokio::time::sleep(timing.interval).await;
        let before = *pongs.borrow_and_update();
        let probe = async {
            conn.sink
                .lock()
                .await
                .sink
                .send(Message::Ping(Vec::new().into()))
                .await
                .map_err(|_| false)?;
            pongs
                .wait_for(|n| *n > before)
                .await
                .map(|_| ())
                .map_err(|_| false)
        };
        match tokio::time::timeout(timing.timeout, probe).await {
            Ok(Ok(())) => continue,
            Ok(Err(_)) => return,
            Err(_) => {
                conn.stale.store(true, Ordering::SeqCst);
                conn.reader.abort();
                return;
            }
        }
    }
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

    fn current_conn(&self) -> Result<Arc<Conn>, RecvError> {
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
                serde_json::to_value(&hello.specs).map_err(|e| e.to_string())?,
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
            crate::slog!("unreadable command reply failed id={id}: {err}");
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
                        crate::slog!("command without a kind id={}", cmd.id);
                        self.reply_unreadable(&cmd.id, UNREADABLE_NO_KIND).await;
                        continue;
                    }
                    Some(cmd) => return Ok(cmd),
                    None => {
                        match salvage_id(&text) {
                            Some(id) => {
                                crate::slog!("unreadable command id={id} len={}", text.len());
                                self.reply_unreadable(&id, UNREADABLE_FRAME).await;
                            }
                            None => crate::slog!("dropped unreadable frame len={}", text.len()),
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
            crate::slog!("resent {flushed} queued result(s) after reconnect");
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
        let conn = self.current_conn().map_err(|e| e.to_string())?;
        let mut writer = conn.sink.lock().await;
        writer.send_text(text).await
    }
}

fn result_payload(id: &str, out: &Outcome) -> Result<Value, String> {
    let mut payload = Map::new();
    payload.insert("id".into(), json!(id));
    payload.insert("ok".into(), json!(out.ok));
    payload.insert("result".into(), out.result.clone());
    payload.insert("error".into(), json!(out.error));
    payload.insert("elapsed_ms".into(), json!(out.elapsed_ms));
    if let Some(detail) = &out.detail {
        payload.insert("detail".into(), detail.clone());
    }
    if !out.clamped.is_empty() {
        payload.insert(
            "clamped".into(),
            serde_json::to_value::<&Vec<Clamp>>(&out.clamped).map_err(|e| e.to_string())?,
        );
    }
    if !out.ignored.is_empty() {
        payload.insert("ignored".into(), json!(out.ignored));
    }
    Ok(Value::Object(payload))
}

fn frame_text(kind: &str, payload: Value) -> Result<String, String> {
    let event = TelemetryEvent {
        ts: util::now_ms(),
        kind,
        payload,
    };
    serde_json::to_string(&event).map_err(|e| e.to_string())
}

fn salvage_id(text: &str) -> Option<String> {
    let at = text.find("\"id\"")?;
    let rest = text[at + 4..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let id = &rest[..end];
    if id.is_empty() || id.len() > 128 || id.contains('\\') {
        return None;
    }
    Some(id.to_string())
}

async fn close_conn(conn: &Conn, reason: &'static str) {
    let closing = async {
        let mut writer = conn.sink.lock().await;
        let _ = writer
            .sink
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: reason.into(),
            })))
            .await;
    };
    let _ = tokio::time::timeout(Duration::from_secs(1), closing).await;
    conn.reader.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{Param, ParamType};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    struct TestServer {
        url: String,
        listener: TcpListener,
    }

    async fn server() -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        TestServer { url, listener }
    }

    #[allow(clippy::result_large_err)]
    async fn accept(listener: &TcpListener) -> (WsStream2, String) {
        let (tcp, _) = listener.accept().await.unwrap();
        let token = Arc::new(Mutex::new(String::new()));
        let seen = token.clone();
        let ws = tokio_tungstenite::accept_hdr_async(tcp, move |req: &Request, resp: Response| {
            if let Some(v) = req.headers().get("X-Write-Token") {
                *seen.lock().unwrap() = v.to_str().unwrap().to_string();
            }
            Ok(resp)
        })
        .await
        .unwrap();
        let tok = token.lock().unwrap().clone();
        (ws, tok)
    }

    type WsStream2 = WebSocketStream<TcpStream>;

    async fn next_text(ws: &mut WsStream2) -> Value {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Text(t) => return serde_json::from_str(&t).unwrap(),
                _ => continue,
            }
        }
    }

    fn token() -> CancellationToken {
        CancellationToken::new()
    }

    async fn within<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("timed out")
    }

    #[test]
    fn command_parse_captures_timeout_outside_extras() {
        let cmd = Command::parse(
            r#"{"id":"cmd-1","kind":"ps.exec","timeout_ms":1500,"script":"Start-Sleep 30"}"#,
        )
        .unwrap();
        assert_eq!(cmd.id, "cmd-1");
        assert_eq!(cmd.kind, "ps.exec");
        assert_eq!(cmd.timeout_ms, 1500);
        assert!(!cmd.extras.contains_key("timeout_ms"));
        assert!(cmd.extras.contains_key("script"));
    }

    #[test]
    fn command_parse_discards_garbage() {
        assert!(Command::parse("not json").is_none());
        assert!(Command::parse("[1,2]").is_none());
        assert!(Command::parse("{}").is_some());
    }

    #[test]
    fn hello_stays_under_the_frame_budget() {
        let specs: Vec<Spec> = (0..80)
            .map(|_| Spec {
                kind: "group.kind-with-a-fairly-long-name".into(),
                label: "A reasonably descriptive label".into(),
                description: "A one-line description of what this command does on the host.".into(),
                params: vec![
                    Param::new("path", ParamType::String)
                        .required()
                        .describe("Absolute path to read."),
                    Param::new("limit", ParamType::Int)
                        .default_value(json!(200))
                        .range(1, 2000),
                    Param::new("mode", ParamType::Enum).enum_of(&["contains", "prefix", "regex"]),
                ],
                ..Spec::default()
            })
            .collect();
        let enc = serde_json::to_vec(&specs).unwrap();
        assert!(
            enc.len() <= 64 * 1024,
            "command_specs is {} bytes",
            enc.len()
        );
    }

    #[test]
    fn http_to_ws_maps_schemes() {
        assert_eq!(
            http_to_ws("http://127.0.0.1:5099").unwrap(),
            "ws://127.0.0.1:5099/"
        );
        assert_eq!(
            http_to_ws("https://example.test").unwrap(),
            "wss://example.test/"
        );
        assert!(http_to_ws("ftp://example.test").is_err());
    }

    #[test]
    fn close_error_text_carries_code_and_reason() {
        let err = RecvError::Closed {
            code: 1001,
            reason: "cf timeout".into(),
        };
        let text = err.to_string();
        assert!(err.is_eof());
        assert!(
            text.contains("1001") && text.contains("cf timeout"),
            "{text}"
        );
        assert!(text.contains("StatusGoingAway"), "{text}");
    }

    #[tokio::test]
    async fn bridge_round_trips_against_a_local_relay() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let (mut ws, tok) = accept(&listener).await;
            let hello = next_text(&mut ws).await;
            let tunnel = next_text(&mut ws).await;
            let stream_close = next_text(&mut ws).await;
            let close = next_text(&mut ws).await;
            let result = next_text(&mut ws).await;
            (tok, hello, tunnel, stream_close, close, result)
        });

        let cancel = token();
        let bridge = within(Bridge::dial(&url, "write-token", &cancel))
            .await
            .unwrap();
        let specs = vec![Spec {
            kind: "app.list".into(),
            params: vec![Param::new("limit", ParamType::Int)
                .default_value(json!(300))
                .range(1, 5000)],
            ..Spec::default()
        }];
        within(bridge.send_hello(
            "host-a",
            "v2026.5.22.3",
            &["sys.uptime".to_string(), "fs.read".to_string()],
            &specs,
        ))
        .await
        .unwrap();
        within(bridge.send_tunnel_data("tn1", "s1", b"hello"))
            .await
            .unwrap();
        within(bridge.send_tunnel_stream_close("tn1", "s1", "eof"))
            .await
            .unwrap();
        within(bridge.send_tunnel_close("tn1", "host shutdown"))
            .await
            .unwrap();
        within(bridge.send_command_result(
            "c1",
            &Outcome {
                ok: true,
                result: json!({"a": 1}),
                elapsed_ms: 7,
                clamped: vec![Clamp {
                    param: "limit".into(),
                    from: 9,
                    to: 5,
                }],
                ignored: vec!["typo".into()],
                detail: Some(json!({})),
                ..Outcome::default()
            },
        ))
        .await
        .unwrap();

        let (tok, hello, tunnel, stream_close, close, result) = within(accepting).await.unwrap();
        assert_eq!(tok, "write-token");

        assert_eq!(hello["kind"], "hello");
        let p = &hello["payload"];
        assert_eq!(p["hostname"], "host-a");
        assert_eq!(p["version"], "v2026.5.22.3");
        assert_eq!(p["os"], "windows");
        assert_eq!(p["protocol_version"], 2);
        assert_eq!(p["capabilities"], json!(["fs.read", "sys.uptime"]));
        assert_eq!(p["command_specs"][0]["kind"], "app.list");
        assert_eq!(p["command_specs"][0]["params"][0]["max"], 5000);

        assert_eq!(tunnel["kind"], "tunnel_data");
        assert_eq!(tunnel["payload"]["tunnel_id"], "tn1");
        assert_eq!(tunnel["payload"]["stream_id"], "s1");
        assert_eq!(tunnel["payload"]["data_base64"], "aGVsbG8=");
        assert_eq!(stream_close["kind"], "tunnel_stream_close");
        assert_eq!(close["kind"], "tunnel_close");

        assert_eq!(result["kind"], "command_result");
        let rp = &result["payload"];
        assert_eq!(rp["id"], "c1");
        assert_eq!(rp["ok"], true);
        assert_eq!(rp["result"], json!({"a": 1}));
        assert_eq!(rp["error"], "");
        assert_eq!(rp["elapsed_ms"], 7);
        assert_eq!(rp["detail"], json!({}));
        assert_eq!(
            rp["clamped"],
            json!([{"param": "limit", "from": 9, "to": 5}])
        );
        assert_eq!(rp["ignored"], json!(["typo"]));
        bridge.close().await;
    }

    #[tokio::test]
    async fn hello_without_specs_omits_command_specs() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let (mut ws, _) = accept(&listener).await;
            next_text(&mut ws).await
        });
        let bridge = within(Bridge::dial(&url, "t", &token())).await.unwrap();
        within(bridge.send_hello("h", "v", &["a.b".to_string()], &[]))
            .await
            .unwrap();
        let hello = within(accepting).await.unwrap();
        assert!(hello["payload"].get("command_specs").is_none());
        let frame = serde_json::to_string(&hello).unwrap();
        assert!(frame.starts_with(r#"{"kind":"hello""#) || frame.contains("payload"));
    }

    #[tokio::test]
    async fn frames_serialize_ts_kind_payload_in_order() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let (mut ws, _) = accept(&listener).await;
            loop {
                if let Message::Text(t) = ws.next().await.unwrap().unwrap() {
                    return t.to_string();
                }
            }
        });
        let bridge = within(Bridge::dial(&url, "t", &token())).await.unwrap();
        within(bridge.send("ping", Value::Null)).await.unwrap();
        let raw = within(accepting).await.unwrap();
        assert!(raw.starts_with(r#"{"ts":"#), "{raw}");
        assert!(raw.ends_with(r#","kind":"ping","payload":null}"#), "{raw}");
    }

    #[tokio::test]
    async fn reconnect_swaps_conn_and_replays_hello() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let mut frames = Vec::new();
            for _ in 0..2 {
                let (mut ws, tok) = accept(&listener).await;
                frames.push((tok, next_text(&mut ws).await));
            }
            frames
        });

        let cancel = token();
        let bridge = within(Bridge::dial(&url, "write-token", &cancel))
            .await
            .unwrap();
        within(bridge.send_hello(
            "host-a",
            "v2026.6.17.2",
            &["sys.info".to_string(), "tunnel.open".to_string()],
            &[],
        ))
        .await
        .unwrap();
        within(bridge.reconnect(&cancel)).await.unwrap();

        let frames = within(accepting).await.unwrap();
        assert_eq!(frames.len(), 2);
        for (tok, event) in frames {
            assert_eq!(tok, "write-token");
            assert_eq!(event["kind"], "hello");
            assert_eq!(event["payload"]["hostname"], "host-a");
            assert_eq!(event["payload"]["version"], "v2026.6.17.2");
            assert_eq!(
                event["payload"]["capabilities"],
                json!(["sys.info", "tunnel.open"])
            );
        }
        bridge.close().await;
    }

    #[tokio::test]
    async fn reconnect_honors_cancelled_context() {
        let cancel = token();
        cancel.cancel();
        let bridge = Bridge {
            inner: Arc::new(Inner::new(
                "http://127.0.0.1:1",
                "write-token",
                KeepaliveTiming::default(),
                None,
            )),
        };
        assert!(bridge.reconnect(&cancel).await.is_err());
    }

    fn fast() -> KeepaliveTiming {
        KeepaliveTiming {
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(200),
        }
    }

    async fn silent_server() -> (String, JoinHandle<()>) {
        let TestServer { url, listener } = server().await;
        let handle = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                held.push(ws);
            }
        });
        (url, handle)
    }

    #[tokio::test]
    async fn keepalive_detects_half_open_conn() {
        let (url, server) = silent_server().await;
        let cancel = token();
        let bridge = within(Bridge::dial_with(&url, "write-token", &cancel, fast()))
            .await
            .unwrap();
        let err = within(bridge.recv(&cancel)).await.unwrap_err();
        assert_eq!(err, RecvError::Stale);
        bridge.close().await;
        server.abort();
    }

    #[tokio::test]
    async fn reconnect_stops_old_keepalive() {
        let (silent_url, silent) = silent_server().await;
        let TestServer {
            url: live_url,
            listener,
        } = server().await;
        let live = tokio::spawn(async move {
            let (mut ws, _) = accept(&listener).await;
            let sender = tokio::time::sleep(Duration::from_millis(400));
            tokio::pin!(sender);
            let mut sent = false;
            loop {
                tokio::select! {
                    _ = &mut sender, if !sent => {
                        sent = true;
                        ws.send(Message::text(r#"{"id":"c1","kind":"noop"}"#)).await.unwrap();
                    }
                    msg = ws.next() => {
                        if msg.is_none() { return; }
                    }
                }
            }
        });

        let cancel = token();
        let bridge = within(Bridge::dial_with(
            &silent_url,
            "write-token",
            &cancel,
            fast(),
        ))
        .await
        .unwrap();
        assert_eq!(
            within(bridge.recv(&cancel)).await.unwrap_err(),
            RecvError::Stale
        );

        let moved = Bridge {
            inner: Arc::new(Inner::new(
                &live_url,
                "write-token",
                fast(),
                bridge.current_conn().ok(),
            )),
        };
        within(moved.reconnect(&cancel)).await.unwrap();
        let cmd = within(moved.recv(&cancel)).await.unwrap();
        assert_eq!(cmd.id, "c1");
        assert_eq!(cmd.kind, "noop");
        moved.close().await;
        silent.abort();
        live.abort();
    }

    #[tokio::test]
    async fn recv_preserves_close_code() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let (mut ws, _) = accept(&listener).await;
            ws.close(Some(CloseFrame {
                code: CloseCode::Away,
                reason: "cf timeout".into(),
            }))
            .await
            .unwrap();
            while ws.next().await.is_some() {}
        });
        let cancel = token();
        let bridge = within(Bridge::dial(&url, "write-token", &cancel))
            .await
            .unwrap();
        let err = within(bridge.recv(&cancel)).await.unwrap_err();
        assert!(err.is_eof(), "{err:?}");
        let text = err.to_string();
        assert!(
            text.contains("1001") && text.contains("cf timeout"),
            "{text}"
        );
        accepting.abort();
    }

    #[tokio::test]
    async fn recv_skips_garbage_frames_and_honors_cancel() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let (mut ws, _) = accept(&listener).await;
            ws.send(Message::text("not json")).await.unwrap();
            ws.send(Message::text(r#"{"id":"c9","kind":"sys.info","x":1}"#))
                .await
                .unwrap();
            while ws.next().await.is_some() {}
        });
        let cancel = token();
        let bridge = within(Bridge::dial(&url, "t", &cancel)).await.unwrap();
        let cmd = within(bridge.recv(&cancel)).await.unwrap();
        assert_eq!(cmd.id, "c9");
        assert_eq!(cmd.extras["x"], json!(1));

        cancel.cancel();
        assert_eq!(
            within(bridge.recv(&cancel)).await.unwrap_err(),
            RecvError::Cancelled
        );
        accepting.abort();
    }

    #[tokio::test]
    async fn hello_carries_presence_beside_a_flat_capability_list() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let (mut ws, _) = accept(&listener).await;
            next_text(&mut ws).await
        });
        let bridge = within(Bridge::dial(&url, "t", &token())).await.unwrap();
        bridge.want_presence();
        within(bridge.send_hello("h", "v", &["b.x".to_string(), "a.y".to_string()], &[]))
            .await
            .unwrap();
        let hello = within(accepting).await.unwrap();
        assert_eq!(hello["payload"]["presence"], json!(true));
        assert_eq!(hello["payload"]["capabilities"], json!(["a.y", "b.x"]));
    }

    #[tokio::test]
    async fn hello_leaves_presence_out_unless_asked() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let (mut ws, _) = accept(&listener).await;
            next_text(&mut ws).await
        });
        let bridge = within(Bridge::dial(&url, "t", &token())).await.unwrap();
        within(bridge.send_hello("h", "v", &[], &[])).await.unwrap();
        let hello = within(accepting).await.unwrap();
        assert!(hello["payload"].get("presence").is_none());
    }

    #[tokio::test]
    async fn result_sent_while_offline_is_resent_after_reconnect() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let (mut ws, _) = accept(&listener).await;
            let hello = next_text(&mut ws).await;
            let result = next_text(&mut ws).await;
            (hello, result)
        });
        let bridge = Bridge {
            inner: Arc::new(Inner::new(&url, "t", KeepaliveTiming::default(), None)),
        };
        let _ = bridge.send_hello("h", "v", &[], &[]).await;
        let out = Outcome {
            ok: true,
            result: json!({"done": 1}),
            ..Outcome::default()
        };
        assert_eq!(
            bridge
                .send_command_result_or_queue("c7", &out)
                .await
                .unwrap(),
            Delivery::Queued
        );
        assert_eq!(bridge.pending_results().await, 1);
        within(bridge.reconnect(&token())).await.unwrap();
        let (hello, result) = within(accepting).await.unwrap();
        assert_eq!(hello["kind"], "hello");
        assert_eq!(result["kind"], "command_result");
        assert_eq!(result["payload"]["id"], "c7");
        assert_eq!(result["payload"]["result"], json!({"done": 1}));
        assert_eq!(bridge.pending_results().await, 0);
        bridge.close().await;
    }

    #[tokio::test]
    async fn queue_keeps_only_the_newest_results() {
        let bridge = Bridge {
            inner: Arc::new(Inner::new(
                "http://127.0.0.1:1",
                "t",
                KeepaliveTiming::default(),
                None,
            )),
        };
        for i in 0..PENDING_RESULTS_CAP + 5 {
            let _ = bridge
                .send_command_result_or_queue(&format!("c{i}"), &Outcome::default())
                .await;
        }
        assert_eq!(bridge.pending_results().await, PENDING_RESULTS_CAP);
        let first = bridge.inner.pending.lock().await.front().cloned().unwrap();
        assert!(first.contains("\"id\":\"c5\""), "{first}");
    }

    #[tokio::test]
    async fn unreadable_command_with_an_id_gets_an_error_reply() {
        let TestServer { url, listener } = server().await;
        let accepting = tokio::spawn(async move {
            let (mut ws, _) = accept(&listener).await;
            ws.send(Message::text(r#"{"id":"c3","kind":"fs.read","args":{"#))
                .await
                .unwrap();
            ws.send(Message::text(r#"{"id":"c4","timeout_ms":5}"#))
                .await
                .unwrap();
            ws.send(Message::text(r#"{"id":"c5","kind":"sys.info"}"#))
                .await
                .unwrap();
            let a = next_text(&mut ws).await;
            let b = next_text(&mut ws).await;
            while ws.next().await.is_some() {}
            (a, b)
        });
        let cancel = token();
        let bridge = within(Bridge::dial(&url, "t", &cancel)).await.unwrap();
        let cmd = within(bridge.recv(&cancel)).await.unwrap();
        assert_eq!(cmd.id, "c5");
        let (a, b) = within(async {
            bridge.close().await;
            accepting.await.unwrap()
        })
        .await;
        assert_eq!(a["payload"]["id"], "c3");
        assert_eq!(a["payload"]["ok"], false);
        assert_eq!(a["payload"]["error"], UNREADABLE_FRAME);
        assert_eq!(b["payload"]["id"], "c4");
        assert_eq!(b["payload"]["error"], UNREADABLE_NO_KIND);
    }

    #[test]
    fn salvage_id_finds_the_id_in_broken_json() {
        assert_eq!(
            salvage_id(r#"{"id": "c1", "kind": "#).as_deref(),
            Some("c1")
        );
        assert_eq!(salvage_id(r#"{"kind":"x""#), None);
        assert_eq!(salvage_id(r#"{"id":5}"#), None);
        assert_eq!(salvage_id("not json"), None);
    }
}

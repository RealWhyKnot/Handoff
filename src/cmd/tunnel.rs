use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{FutureExt, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use super::connect::default_relay;
use super::new::{sleep_cancel, ReconnectBackoff};
use crate::slog;
use crate::zwire::{self, Packer, Unpacker};

pub const DEFAULT_LOCAL_PORT: u16 = 47800;
pub const PORT_SCAN_RANGE: u32 = 20;

pub const USAGE: &str =
    "usage: handoff tunnel <connect-token> [--local-port PORT] [--relay URL] [--http-host HOST]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelOptions {
    pub token: String,
    pub local_port: u16,
    pub relay: String,
    pub http_host: String,
    pub port_explicit: bool,
}

fn parse_port(raw: &str, shown: &str) -> Result<u16, String> {
    match raw.parse::<i64>() {
        Ok(p) if p > 0 && p <= 65535 => Ok(p as u16),
        _ => Err(format!(
            "invalid --local-port {}",
            crate::util::go_quote(shown)
        )),
    }
}

pub fn clean_http_host(host: &str) -> Result<String, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("--http-host requires a non-empty value".to_string());
    }
    if host.contains(['\r', '\n']) {
        return Err("--http-host must not contain newlines".to_string());
    }
    Ok(host.to_string())
}

pub fn parse_tunnel_args(args: &[String]) -> Result<TunnelOptions, String> {
    parse_tunnel_args_with(args, &default_relay())
}

pub fn parse_tunnel_args_with(
    args: &[String],
    default_relay: &str,
) -> Result<TunnelOptions, String> {
    let mut local_port: u16 = 0;
    let mut relay = default_relay.to_string();
    let mut http_host = String::new();
    let mut token = String::new();
    let mut positional = 0;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let value_of = |name: &str, i: &mut usize| -> Result<String, String> {
            if *i + 1 >= args.len() {
                return Err(format!("{name} requires a value"));
            }
            *i += 1;
            Ok(args[*i].clone())
        };
        match a {
            "--local-port" | "-p" => {
                let v = value_of("--local-port", &mut i)?;
                local_port = parse_port(&v, &v)?;
            }
            "--relay" => relay = value_of("--relay", &mut i)?,
            "--http-host" | "--host-header" => {
                let v = value_of("--http-host", &mut i)?;
                http_host = clean_http_host(&v)?;
            }
            _ if a.starts_with("--local-port=") => {
                local_port = parse_port(&a["--local-port=".len()..], a)?;
            }
            _ if a.starts_with("--relay=") => relay = a["--relay=".len()..].to_string(),
            _ if a.starts_with("--http-host=") => {
                http_host = clean_http_host(&a["--http-host=".len()..])?;
            }
            _ if a.starts_with("--host-header=") => {
                http_host = clean_http_host(&a["--host-header=".len()..])?;
            }
            _ if a.starts_with('-') => {
                return Err(format!("unknown flag {}", crate::util::go_quote(a)));
            }
            _ => {
                if positional == 0 {
                    token = a.to_string();
                    positional += 1;
                } else {
                    return Err(format!("unexpected argument {}", crate::util::go_quote(a)));
                }
            }
        }
        i += 1;
    }

    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("connect token is required".to_string());
    }
    let port_explicit = local_port != 0;
    if local_port == 0 {
        local_port = DEFAULT_LOCAL_PORT;
    }
    Ok(TunnelOptions {
        token,
        local_port,
        relay,
        http_host,
        port_explicit,
    })
}

pub fn bind_local_listener(port: u16, explicit: bool) -> std::io::Result<TcpListener> {
    let attempts = if explicit { 1 } else { PORT_SCAN_RANGE };
    let mut last_err: Option<std::io::Error> = None;
    for offset in 0..attempts {
        let candidate = port as u32 + offset;
        if candidate > 65535 {
            break;
        }
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, candidate as u16));
        match TcpListener::bind(addr) {
            Ok(listener) => return Ok(listener),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no port available")))
}

pub fn is_tunnel_url(token: &str) -> bool {
    ["http://", "https://", "ws://", "wss://"]
        .iter()
        .any(|p| token.starts_with(p))
}

fn swap_scheme(mut u: url::Url) -> url::Url {
    let target = match u.scheme() {
        "http" => Some("ws"),
        "https" => Some("wss"),
        _ => None,
    };
    if let Some(scheme) = target {
        let _ = u.set_scheme(scheme);
    }
    u
}

pub fn tunnel_ws_url(relay_base: &str, token: &str) -> Result<String, String> {
    if is_tunnel_url(token) {
        let u = url::Url::parse(token).map_err(|e| e.to_string())?;
        return Ok(swap_scheme(u).to_string());
    }
    let base = relay_base.trim_end_matches('/');
    let u = url::Url::parse(&format!("{base}/api/tunnel/{token}")).map_err(|e| e.to_string())?;
    Ok(swap_scheme(u).to_string())
}

pub const READY_TIMEOUT: Duration = Duration::from_secs(15);
pub const SESSION_DIAL_TIMEOUT: Duration = Duration::from_secs(15);
pub const READ_LIMIT: usize = 8 << 20;
pub const HTTP_HEADER_REWRITE_LIMIT: usize = 64 * 1024;

const HTTP_METHOD_PREFIXES: [&[u8]; 8] = [
    b"GET ",
    b"POST ",
    b"HEAD ",
    b"PUT ",
    b"DELETE ",
    b"OPTIONS ",
    b"PATCH ",
    b"TRACE ",
];

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<WsStream, Message>;
type WsSource = SplitStream<WsStream>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelReady {
    pub tunnel_id: String,
    pub host_addr: String,
    pub host_port: i64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct WireFrame {
    #[serde(default, deserialize_with = "null_default")]
    kind: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    stream_id: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    tunnel_id: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    data_base64: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    reason: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    message: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    host_addr: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "is_zero"
    )]
    host_port: i64,
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl WireFrame {
    fn kind(kind: &str) -> WireFrame {
        WireFrame {
            kind: kind.to_string(),
            ..WireFrame::default()
        }
    }
}

enum LoopEnd {
    ClosedByRelay,
    Cancelled,
    Failed(String),
}

pub async fn run_sessions(
    options: TunnelOptions,
    ws_url: String,
    listener: TcpListener,
    bound_port: u16,
    cancel: CancellationToken,
) -> Result<(), String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let listener =
        Arc::new(tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?);
    let mut backoff = ReconnectBackoff::new();
    let mut announced = false;
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let step = AssertUnwindSafe(serve_session(
            &cancel,
            &options,
            &ws_url,
            &listener,
            bound_port,
            &mut announced,
        ))
        .catch_unwind()
        .await;
        let (done, err) = match step {
            Ok(v) => v,
            Err(payload) => {
                let text = panic_text(payload.as_ref());
                slog!("tunnel session panic: {text}");
                (false, Some(format!("internal error: {text}")))
            }
        };
        if done || cancel.is_cancelled() {
            return Ok(());
        }
        if let Some(err) = err {
            slog!("tunnel session ended: {err}");
            eprintln!("tunnel connection lost: {err}");
        }
        println!("reconnecting...");
        if !sleep_cancel(&cancel, backoff.next_delay()).await {
            return Ok(());
        }
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "unknown panic".to_string()
}

async fn serve_session(
    cancel: &CancellationToken,
    opts: &TunnelOptions,
    ws_url: &str,
    listener: &Arc<tokio::net::TcpListener>,
    bound_port: u16,
    announced: &mut bool,
) -> (bool, Option<String>) {
    crate::util::install_crypto_provider();
    let config = WebSocketConfig::default()
        .max_message_size(Some(READ_LIMIT))
        .max_frame_size(Some(READ_LIMIT));
    let mut request = match ws_url.into_client_request() {
        Ok(r) => r,
        Err(err) => return (false, Some(err.to_string())),
    };
    request
        .headers_mut()
        .insert(zwire::HEADER, HeaderValue::from_static(zwire::ZSTD));
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), false);
    let (stream, zipped) = tokio::select! {
        _ = cancel.cancelled() => return (false, Some("context canceled".to_string())),
        res = tokio::time::timeout(SESSION_DIAL_TIMEOUT, connect) => match res {
            Ok(Ok((s, resp))) => (s, zwire::offers_zstd(resp.headers().get(zwire::HEADER).and_then(|v| v.to_str().ok()))),
            Ok(Err(err)) => return (false, Some(err.to_string())),
            Err(_) => return (false, Some("context deadline exceeded".to_string())),
        },
    };
    let (zip, mut unzip) = if zipped {
        match (Packer::new(zwire::LEVEL), Unpacker::new(READ_LIMIT)) {
            (Ok(p), Ok(u)) => (Some(p), Some(u)),
            (Err(err), _) | (_, Err(err)) => return (false, Some(err.to_string())),
        }
    } else {
        (None, None)
    };
    let (sink, mut source) = stream.split();

    let ready = match await_tunnel_ready(cancel, &mut source, &mut unzip).await {
        Ok(r) => r,
        Err(err) => return (false, Some(err)),
    };

    let http_host = if opts.http_host.is_empty() {
        default_tunnel_http_host(&ready.host_addr, ready.host_port)
    } else {
        opts.http_host.clone()
    };
    println!(
        "tunnel ready: forwarding 127.0.0.1:{} -> host {}:{}",
        bound_port, ready.host_addr, ready.host_port
    );
    if !*announced {
        if !http_host.is_empty() {
            println!("HTTP Host headers will use {http_host}");
        }
        println!("open http://127.0.0.1:{bound_port}/ in your browser");
        println!("press Ctrl+C to close the tunnel.");
        *announced = true;
    }

    let client = Arc::new(TunnelClient::new(sink, zip, http_host));
    let session = cancel.child_token();
    tokio::spawn(
        client
            .clone()
            .accept_loop(session.clone(), listener.clone()),
    );

    let end = client.read_loop(&session, &mut source, &mut unzip).await;
    session.cancel();
    client.shutdown("session ended").await;
    drop(source);

    match end {
        LoopEnd::ClosedByRelay => (true, None),
        LoopEnd::Failed(err) if !cancel.is_cancelled() => (false, Some(err)),
        _ => (cancel.is_cancelled(), None),
    }
}

fn inflate(msg: Message, unzip: &mut Option<Unpacker>) -> Result<Message, String> {
    match (msg, unzip.as_mut()) {
        (Message::Binary(b), Some(u)) => {
            let raw = u
                .unpack(&b)
                .map_err(|e| format!("bad compressed frame: {e}"))?;
            String::from_utf8(raw)
                .map(Message::text)
                .map_err(|e| format!("bad compressed frame: {e}"))
        }
        (m, _) => Ok(m),
    }
}

async fn await_tunnel_ready(
    cancel: &CancellationToken,
    source: &mut WsSource,
    unzip: &mut Option<Unpacker>,
) -> Result<TunnelReady, String> {
    let wait = async {
        loop {
            let msg = match source.next().await {
                Some(Ok(m)) => inflate(m, unzip)?,
                Some(Err(err)) => return Err(err.to_string()),
                None => return Err("EOF".to_string()),
            };
            let Message::Text(text) = msg else {
                if let Message::Close(frame) = msg {
                    return Err(close_text(
                        frame.map(|f| (u16::from(f.code), f.reason.to_string())),
                    ));
                }
                continue;
            };
            let Ok(frame) = serde_json::from_str::<WireFrame>(&text) else {
                continue;
            };
            match frame.kind.as_str() {
                "tunnel_ready" => {
                    let host = if frame.host_addr.is_empty() {
                        "127.0.0.1".to_string()
                    } else {
                        frame.host_addr
                    };
                    return Ok(TunnelReady {
                        tunnel_id: frame.tunnel_id,
                        host_addr: host,
                        host_port: frame.host_port,
                    });
                }
                "tunnel_closed" => return Err(format!("relay closed tunnel: {}", frame.reason)),
                "error" => return Err(format!("relay refused tunnel: {}", frame.message)),
                _ => {}
            }
        }
    };
    tokio::select! {
        _ = cancel.cancelled() => Err("context canceled".to_string()),
        res = tokio::time::timeout(READY_TIMEOUT, wait) => match res {
            Ok(r) => r,
            Err(_) => Err("context deadline exceeded".to_string()),
        },
    }
}

fn close_text(frame: Option<(u16, String)>) -> String {
    match frame {
        Some((code, reason)) => {
            format!("received close frame: status = {code} and reason = {reason:?}")
        }
        None => "EOF".to_string(),
    }
}

struct StreamEntry {
    writer: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    cancel: CancellationToken,
}

struct TunnelClient {
    sink: tokio::sync::Mutex<Option<(WsSink, Option<Packer>)>>,
    http_host: String,
    streams: Mutex<HashMap<String, StreamEntry>>,
}

impl TunnelClient {
    fn new(sink: WsSink, zip: Option<Packer>, http_host: String) -> TunnelClient {
        TunnelClient {
            sink: tokio::sync::Mutex::new(Some((sink, zip))),
            http_host,
            streams: Mutex::new(HashMap::new()),
        }
    }

    fn streams(&self) -> std::sync::MutexGuard<'_, HashMap<String, StreamEntry>> {
        self.streams.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn accept_loop(
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
                    slog!("tunnel-client accept error: {err}");
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
            tokio::spawn(self.clone().copy_to_relay(stream_id, rd, stream_cancel));
        }
    }

    async fn read_loop(
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
                    ))
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
                            slog!("tunnel-client decode data error: {err}");
                            continue;
                        }
                    };
                    self.write_to_stream(&frame.stream_id, &payload).await;
                }
                "stream_close" => self.drop_stream(&frame.stream_id, &frame.reason),
                "tunnel_closed" => {
                    println!("relay closed tunnel: {}", frame.reason);
                    return LoopEnd::ClosedByRelay;
                }
                "error" => eprintln!("relay error: {}", frame.message),
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
                slog!(
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
                        slog!("tunnel-client local read error stream={stream_id}: {err}");
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
        let data = serde_json::to_string(frame).map_err(|e| e.to_string())?;
        let mut sink = self.sink.lock().await;
        match sink.as_mut() {
            Some((s, Some(z))) => {
                let packed = z.pack(data.as_bytes()).map_err(|e| e.to_string())?;
                s.send(Message::binary(packed))
                    .await
                    .map_err(|e| e.to_string())
            }
            Some((s, None)) => s.send(Message::text(data)).await.map_err(|e| e.to_string()),
            None => Err("tunnel client closed".to_string()),
        }
    }

    async fn write_to_stream(&self, stream_id: &str, payload: &[u8]) {
        let writer = match self.streams().get(stream_id) {
            Some(e) => e.writer.clone(),
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
            slog!("tunnel-client stream end stream={stream_id} reason={reason}");
        }
    }

    async fn shutdown(&self, reason: &str) {
        self.sink.lock().await.take();
        let entries: Vec<StreamEntry> = self.streams().drain().map(|(_, e)| e).collect();
        for e in entries {
            e.cancel.cancel();
        }
        let _ = reason;
    }
}

pub fn default_tunnel_http_host(host: &str, port: i64) -> String {
    let host = host.trim();
    if host.is_empty() || is_loopback_tunnel_host(host) {
        return String::new();
    }
    if port <= 0 || port == 80 {
        return clean_http_host(host).unwrap_or_default();
    }
    let host_port = if host.parse::<IpAddr>().is_ok() && host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    clean_http_host(&host_port).unwrap_or_default()
}

fn is_loopback_tunnel_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

pub fn rewrite_http_host_header(data: &[u8], host: &str) -> Option<Vec<u8>> {
    if host.is_empty() {
        return Some(data.to_vec());
    }
    if !has_http_method_prefix(data) {
        if may_become_http_method(data) {
            return None;
        }
        return Some(data.to_vec());
    }
    let Some(header_end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
        if data.len() < HTTP_HEADER_REWRITE_LIMIT {
            return None;
        }
        return Some(data.to_vec());
    };

    let header = &data[..header_end];
    let body = &data[header_end + 4..];
    let lines = split_crlf(header);

    let host_line = format!("Host: {host}").into_bytes();
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(lines.len() + 1);
    out.push(lines[0].to_vec());
    let mut replaced = false;
    for line in &lines[1..] {
        if line.len() >= 5 && line[..5].eq_ignore_ascii_case(b"host:") {
            if !replaced {
                out.push(host_line.clone());
                replaced = true;
            }
            continue;
        }
        out.push(line.to_vec());
    }
    if !replaced {
        out.insert(1, host_line);
    }

    let mut rewritten = out.join(&b"\r\n"[..]);
    rewritten.extend_from_slice(b"\r\n\r\n");
    rewritten.extend_from_slice(body);
    Some(rewritten)
}

fn split_crlf(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == b'\r' && data[i + 1] == b'\n' {
            out.push(&data[start..i]);
            i += 2;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(&data[start..]);
    out
}

fn has_http_method_prefix(data: &[u8]) -> bool {
    HTTP_METHOD_PREFIXES.iter().any(|m| data.starts_with(m))
}

fn may_become_http_method(data: &[u8]) -> bool {
    if data.is_empty() || data.len() > b"OPTIONS ".len() {
        return false;
    }
    HTTP_METHOD_PREFIXES
        .iter()
        .any(|m| data.len() <= m.len() && m.starts_with(data))
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

fn short_sid(t: &str) -> &str {
    t.get(..11).unwrap_or(t)
}

pub fn tunnel(args: &[String]) -> i32 {
    let opts = match parse_tunnel_args(args) {
        Ok(o) => o,
        Err(err) => {
            eprintln!("handoff tunnel: {err}");
            eprintln!("{USAGE}");
            return 2;
        }
    };

    slog!(
        "tunnel-client start token={} local={} relay={}",
        short_sid(&opts.token),
        opts.local_port,
        opts.relay
    );

    let ws_url = match tunnel_ws_url(&opts.relay, &opts.token) {
        Ok(u) => u,
        Err(err) => {
            eprintln!("handoff tunnel: {err}");
            return 1;
        }
    };

    let listener = match bind_local_listener(opts.local_port, opts.port_explicit) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("could not bind local port: {err}");
            if !opts.port_explicit {
                eprintln!(
                    "tried ports {}-{}; pass --local-port to choose another",
                    opts.local_port,
                    opts.local_port as u32 + PORT_SCAN_RANGE - 1
                );
            }
            return 1;
        }
    };
    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    if bound_port != opts.local_port {
        println!(
            "port {} was busy, using {} instead",
            opts.local_port, bound_port
        );
    }

    println!("connecting to relay: {}", opts.relay);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    let cancel = CancellationToken::new();
    let result = runtime.block_on(async {
        let on_signal = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                println!(
                    "
closing tunnel..."
                );
                slog!("tunnel-client shutdown requested");
                on_signal.cancel();
            }
        });
        run_sessions(opts, ws_url, listener, bound_port, cancel).await
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
    match result {
        Ok(()) => 0,
        Err(err) => {
            slog!("tunnel session ended: {err}");
            eprintln!("handoff tunnel: {err}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn parse(items: &[&str]) -> Result<TunnelOptions, String> {
        parse_tunnel_args_with(&strs(items), "https://handoff.whyknot.dev")
    }

    #[test]
    fn requires_token() {
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn accepts_token_and_port() {
        let opts = parse(&["tk_abc", "--local-port", "1234"]).unwrap();
        assert_eq!(opts.token, "tk_abc");
        assert_eq!(opts.local_port, 1234);
    }

    #[test]
    fn accepts_equals_form() {
        let opts = parse(&[
            "tk_abc",
            "--local-port=5555",
            "--relay=https://example.invalid",
            "--http-host=fritz.repeater",
        ])
        .unwrap();
        assert_eq!(opts.local_port, 5555);
        assert_eq!(opts.relay, "https://example.invalid");
        assert_eq!(opts.http_host, "fritz.repeater");
    }

    #[test]
    fn rejects_bad_port() {
        for bad in ["0", "70000", "notanumber"] {
            assert!(
                parse(&["tk", "--local-port", bad]).is_err(),
                "--local-port {bad} should fail"
            );
        }
    }

    #[test]
    fn rejects_unknown_flag() {
        assert!(parse(&["tk", "--nope"]).is_err());
    }

    #[test]
    fn rejects_bad_http_host() {
        assert!(parse(&["tk", "--http-host", "bad\r\nHost: evil"]).is_err());
    }

    #[test]
    fn rejects_a_second_positional() {
        assert_eq!(
            parse(&["tk", "other"]).unwrap_err(),
            "unexpected argument \"other\""
        );
    }

    #[test]
    fn defaults_local_port() {
        let opts = parse(&["tk_only"]).unwrap();
        assert_eq!(opts.local_port, 47800);
        assert!(!opts.port_explicit);
    }

    #[test]
    fn marks_explicit_port() {
        assert!(
            parse(&["tk", "--local-port", "1234"])
                .unwrap()
                .port_explicit
        );
    }

    #[test]
    fn missing_flag_values_name_the_flag() {
        assert_eq!(
            parse(&["tk", "--relay"]).unwrap_err(),
            "--relay requires a value"
        );
        assert_eq!(
            parse(&["tk", "--local-port"]).unwrap_err(),
            "--local-port requires a value"
        );
    }

    #[test]
    fn bind_rolls_forward_when_port_busy() {
        let busy = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy_port = busy.local_addr().unwrap().port();
        let listener = bind_local_listener(busy_port, false).unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), busy_port);
    }

    #[test]
    fn bind_respects_explicit_port() {
        let busy = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy_port = busy.local_addr().unwrap().port();
        assert!(bind_local_listener(busy_port, true).is_err());
    }

    #[test]
    fn bind_binds_requested_port_when_free() {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let listener = bind_local_listener(port, true).unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    #[test]
    fn ws_url_builds_relay_path() {
        assert_eq!(
            tunnel_ws_url("https://handoff.example/", "tk_abc").unwrap(),
            "wss://handoff.example/api/tunnel/tk_abc"
        );
    }

    #[test]
    fn ws_url_accepts_http_relay() {
        assert_eq!(
            tunnel_ws_url("http://localhost:5099", "tk_abc").unwrap(),
            "ws://localhost:5099/api/tunnel/tk_abc"
        );
    }

    #[test]
    fn ws_url_accepts_full_url() {
        let got = tunnel_ws_url(
            "https://ignored",
            "https://handoff.example/api/tunnel/tk_xyz",
        )
        .unwrap();
        assert!(got.starts_with("wss://handoff.example/"), "{got}");
    }

    #[test]
    fn ws_url_accepts_full_websocket_url() {
        assert_eq!(
            tunnel_ws_url(
                "https://ignored",
                "wss://handoff.example/api/tunnel/n2_tk_xyz"
            )
            .unwrap(),
            "wss://handoff.example/api/tunnel/n2_tk_xyz"
        );
    }

    #[test]
    fn wire_frame_accepts_relay_nulls() {
        let frame: WireFrame = serde_json::from_str(
            r#"{"kind":"data","tunnel_id":"tn1","stream_id":"s_1","data_base64":"b2s=","reason":null,"message":null}"#,
        )
        .unwrap();
        assert_eq!(frame.kind, "data");
        assert_eq!(frame.stream_id, "s_1");
        assert_eq!(frame.data_base64, "b2s=");
        assert_eq!(frame.reason, "");
        let ready: WireFrame = serde_json::from_str(
            r#"{"kind":"tunnel_ready","tunnel_id":"tn1","host_addr":"127.0.0.1","host_port":null}"#,
        )
        .unwrap();
        assert_eq!(ready.host_port, 0);
    }

    #[test]
    fn new_stream_id_format() {
        let id = new_stream_id();
        assert!(id.starts_with("s_"), "{id}");
        assert_eq!(id.len(), 18, "{id}");
        assert!(id[2..].chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        assert_ne!(new_stream_id(), new_stream_id());
    }

    #[test]
    fn default_tunnel_http_host_keeps_hostname() {
        assert_eq!(
            default_tunnel_http_host("fritz.repeater", 80),
            "fritz.repeater"
        );
        assert_eq!(default_tunnel_http_host("127.0.0.1", 80), "");
        assert_eq!(default_tunnel_http_host("localhost", 8080), "");
        assert_eq!(
            default_tunnel_http_host("192.168.2.103", 8080),
            "192.168.2.103:8080"
        );
        assert_eq!(default_tunnel_http_host("fd00::1", 8080), "[fd00::1]:8080");
        assert_eq!(default_tunnel_http_host("", 8080), "");
    }

    #[test]
    fn rewrite_http_host_header_replaces_host() {
        let req = b"GET / HTTP/1.1\r\nHost: 127.0.0.1:18180\r\nUser-Agent: test\r\n\r\n";
        let got = rewrite_http_host_header(req, "fritz.repeater").expect("ready");
        assert_eq!(
            String::from_utf8(got).unwrap(),
            "GET / HTTP/1.1\r\nHost: fritz.repeater\r\nUser-Agent: test\r\n\r\n"
        );
    }

    #[test]
    fn rewrite_http_host_header_inserts_host() {
        let req = b"GET / HTTP/1.1\r\nUser-Agent: test\r\n\r\n";
        let got = rewrite_http_host_header(req, "fritz.repeater").expect("ready");
        assert_eq!(
            String::from_utf8(got).unwrap(),
            "GET / HTTP/1.1\r\nHost: fritz.repeater\r\nUser-Agent: test\r\n\r\n"
        );
    }

    #[test]
    fn rewrite_http_host_header_drops_duplicate_hosts_and_keeps_body() {
        let req = b"POST /x HTTP/1.1\r\nHOST: a\r\nhost: b\r\nContent-Length: 4\r\n\r\nbody";
        let got = rewrite_http_host_header(req, "h").expect("ready");
        assert_eq!(
            String::from_utf8(got).unwrap(),
            "POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: 4\r\n\r\nbody"
        );
    }

    #[test]
    fn rewrite_http_host_header_waits_for_complete_header() {
        assert!(
            rewrite_http_host_header(b"GET / HTTP/1.1\r\nHost: 127", "fritz.repeater").is_none()
        );
        assert!(rewrite_http_host_header(b"GE", "fritz.repeater").is_none());
    }

    #[test]
    fn rewrite_http_host_header_passes_through_non_http() {
        let input = [0x16u8, 0x03, 0x01, 0x00];
        assert_eq!(
            rewrite_http_host_header(&input, "fritz.repeater").unwrap(),
            input
        );
    }

    #[test]
    fn rewrite_http_host_header_gives_up_past_the_limit() {
        let mut big = b"GET / HTTP/1.1\r\nX: ".to_vec();
        big.resize(HTTP_HEADER_REWRITE_LIMIT, b'a');
        assert_eq!(rewrite_http_host_header(&big, "h").unwrap(), big);
    }

    type ServerWs = WebSocketStream<TcpStream>;

    async fn ws_server<F, Fut>(handler: F) -> (String, Arc<AtomicU64>, tokio::task::JoinHandle<()>)
    where
        F: Fn(u64, ServerWs) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/tunnel/tk", listener.local_addr().unwrap());
        let dials = Arc::new(AtomicU64::new(0));
        let counter = dials.clone();
        let handler = Arc::new(handler);
        let handle = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(ws) = tokio_tungstenite::accept_async(tcp).await else {
                    continue;
                };
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::spawn(handler(n, ws));
            }
        });
        (url, dials, handle)
    }

    fn ready_frame() -> Message {
        Message::text(
            serde_json::json!({"kind": "tunnel_ready", "tunnel_id": "tn1", "host_addr": "127.0.0.1", "host_port": 80})
                .to_string(),
        )
    }

    fn opts(http_host: &str) -> TunnelOptions {
        TunnelOptions {
            token: "tk".into(),
            local_port: 0,
            relay: String::new(),
            http_host: http_host.into(),
            port_explicit: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sessions_reconnect_after_relay_drop() {
        let (url, dials, server) = ws_server(|n, mut ws| async move {
            let _ = ws.send(ready_frame()).await;
            if n < 3 {
                let _ = ws
                    .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Error,
                        reason: "drop".into(),
                    }))
                    .await;
                return;
            }
            while let Some(Ok(_)) = ws.next().await {}
        })
        .await;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let run = tokio::spawn(run_sessions(
            opts(""),
            url,
            listener,
            addr.port(),
            cancel.clone(),
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while dials.load(Ordering::SeqCst) < 3 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "relay saw {} dials, want 3",
                dials.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        TcpStream::connect(addr)
            .await
            .expect("listener stopped serving after reconnect");

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("run_sessions did not stop on cancel")
            .unwrap()
            .unwrap();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sessions_stop_on_relay_tunnel_closed() {
        let (url, dials, server) = ws_server(|_, mut ws| async move {
            let _ = ws.send(ready_frame()).await;
            let _ = ws
                .send(Message::text(
                    serde_json::json!({"kind": "tunnel_closed", "reason": "host ended session"})
                        .to_string(),
                ))
                .await;
            while let Some(Ok(_)) = ws.next().await {}
        })
        .await;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let run = tokio::spawn(run_sessions(
            opts(""),
            url,
            listener,
            port,
            CancellationToken::new(),
        ));
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("tunnel_closed did not end the session loop")
            .unwrap()
            .unwrap();
        assert_eq!(dials.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streams_forward_bytes_and_rewrite_the_host_header() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let (url, _, server) = ws_server(move |_, mut ws| {
            let record = record.clone();
            async move {
                let _ = ws.send(ready_frame()).await;
                while let Some(Ok(msg)) = ws.next().await {
                    let Message::Text(text) = msg else { continue };
                    record.lock().unwrap().push(text.to_string());
                    let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if frame["kind"] == "data" {
                        let sid = frame["stream_id"].as_str().unwrap().to_string();
                        let body = base64::engine::general_purpose::STANDARD
                            .encode(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                        let reply = serde_json::json!({"kind": "data", "tunnel_id": "tn1", "stream_id": sid, "data_base64": body});
                        let _ = ws.send(Message::text(reply.to_string())).await;
                        let close = serde_json::json!({"kind": "stream_close", "tunnel_id": "tn1", "stream_id": sid, "reason": "eof"});
                        let _ = ws.send(Message::text(close.to_string())).await;
                    }
                }
            }
        })
        .await;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let run = tokio::spawn(run_sessions(
            opts("fritz.repeater"),
            url,
            listener,
            addr.port(),
            cancel.clone(),
        ));

        let mut conn = TcpStream::connect(addr)
            .await
            .expect("connect to tunnel listener");
        conn.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut got = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut got))
            .await
            .expect("response did not arrive")
            .unwrap();
        assert_eq!(got, b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");

        let frames = seen.lock().unwrap().clone();
        let open: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(open["kind"], "stream_open");
        assert_eq!(open.as_object().unwrap().len(), 2, "{open}");
        let data: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
        let sent = base64::engine::general_purpose::STANDARD
            .decode(data["data_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(sent, b"GET / HTTP/1.1\r\nHost: fritz.repeater\r\n\r\n");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), run).await;
        server.abort();
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)]
    async fn a_relay_that_echoes_zstd_gets_compressed_frames_both_ways() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/tunnel/tk", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let record = seen.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let echo = |req: &Request, mut resp: Response| {
                let asked = zwire::offers_zstd(
                    req.headers()
                        .get(zwire::HEADER)
                        .and_then(|v| v.to_str().ok()),
                );
                if asked {
                    resp.headers_mut()
                        .insert(zwire::HEADER, HeaderValue::from_static(zwire::ZSTD));
                }
                Ok(resp)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(tcp, echo)
                .await
                .unwrap();
            let mut zip = Packer::new(3).unwrap();
            let mut unzip = Unpacker::new(READ_LIMIT).unwrap();
            let mut send =
                |v: serde_json::Value| Message::binary(zip.pack(v.to_string().as_bytes()).unwrap());
            let _ = ws
                .send(send(serde_json::json!({"kind": "tunnel_ready", "tunnel_id": "tn1", "host_addr": "127.0.0.1", "host_port": 80})))
                .await;
            while let Some(Ok(msg)) = ws.next().await {
                let Message::Binary(b) = msg else { continue };
                let frame: serde_json::Value =
                    serde_json::from_slice(&unzip.unpack(&b).unwrap()).unwrap();
                record.lock().unwrap().push(frame.clone());
                if frame["kind"] == "data" {
                    let sid = frame["stream_id"].as_str().unwrap().to_string();
                    let body = base64::engine::general_purpose::STANDARD.encode(
                        b"HTTP/1.1 204 No Content

",
                    );
                    let _ = ws
                        .send(send(serde_json::json!({"kind": "data", "tunnel_id": "tn1", "stream_id": sid, "data_base64": body})))
                        .await;
                    let _ = ws
                        .send(send(serde_json::json!({"kind": "stream_close", "tunnel_id": "tn1", "stream_id": sid, "reason": "eof"})))
                        .await;
                }
            }
        });

        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = tcp.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let run = tokio::spawn(run_sessions(
            opts(""),
            url,
            tcp,
            addr.port(),
            cancel.clone(),
        ));
        let mut conn = TcpStream::connect(addr).await.unwrap();
        conn.write_all(
            b"GET / HTTP/1.1
Host: 127.0.0.1

",
        )
        .await
        .unwrap();
        let mut got = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut got))
            .await
            .expect("response did not arrive")
            .unwrap();
        assert_eq!(
            got,
            b"HTTP/1.1 204 No Content

"
        );
        let kinds: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|f| f["kind"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(&kinds[..2], ["stream_open", "data"]);

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), run).await;
        server.abort();
    }
}

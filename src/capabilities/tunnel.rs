use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::consent::require_risk_consent;
use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};
use crate::relay::Bridge;
use crate::slog;
use crate::util::go_quote;

pub const FRAME_KINDS: [&str; 3] = ["tunnel.stream_open", "tunnel.data", "tunnel.stream_close"];

pub const READ_CHUNK: usize = 16 * 1024;
pub const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;
const _: () = assert!(MAX_FRAME_PAYLOAD >= 64 * 1024);
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
pub const HOST_LOOKUP_TIME: Duration = Duration::from_secs(3);
pub const STREAM_QUEUE_DEPTH: usize = 1024;
pub const WRITE_DEADLINE: Duration = Duration::from_secs(60);

pub trait TunnelBridge: Send + Sync {
    fn send_tunnel_data<'a>(
        &'a self,
        tunnel_id: &'a str,
        stream_id: &'a str,
        data: &'a [u8],
    ) -> BoxFuture<'a, Result<(), String>>;
    fn send_tunnel_stream_close<'a>(
        &'a self,
        tunnel_id: &'a str,
        stream_id: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;
    fn send_tunnel_close<'a>(
        &'a self,
        tunnel_id: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;
}

impl TunnelBridge for Bridge {
    fn send_tunnel_data<'a>(
        &'a self,
        tunnel_id: &'a str,
        stream_id: &'a str,
        data: &'a [u8],
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(Bridge::send_tunnel_data(self, tunnel_id, stream_id, data))
    }

    fn send_tunnel_stream_close<'a>(
        &'a self,
        tunnel_id: &'a str,
        stream_id: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(Bridge::send_tunnel_stream_close(
            self, tunnel_id, stream_id, reason,
        ))
    }

    fn send_tunnel_close<'a>(
        &'a self,
        tunnel_id: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(Bridge::send_tunnel_close(self, tunnel_id, reason))
    }
}

struct NoBridge;

impl TunnelBridge for NoBridge {
    fn send_tunnel_data<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a [u8],
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("bridge is closed".to_string()) })
    }

    fn send_tunnel_stream_close<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("bridge is closed".to_string()) })
    }

    fn send_tunnel_close<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("bridge is closed".to_string()) })
    }
}

pub type Lookup = fn(String) -> BoxFuture<'static, Result<Vec<IpAddr>, String>>;

static MANAGER: Mutex<Option<Arc<TunnelMgr>>> = Mutex::new(None);

fn manager_slot() -> MutexGuard<'static, Option<Arc<TunnelMgr>>> {
    MANAGER.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn active_manager() -> Option<Arc<TunnelMgr>> {
    manager_slot().clone()
}

pub fn set_manager(mgr: Option<Arc<TunnelMgr>>) -> Option<Arc<TunnelMgr>> {
    std::mem::replace(&mut *manager_slot(), mgr)
}

pub fn register(r: &mut Router, bridge: Option<Bridge>) {
    let bridge: Arc<dyn TunnelBridge> = match bridge {
        Some(b) => Arc::new(b),
        None => Arc::new(NoBridge),
    };
    {
        let mut slot = manager_slot();
        if let Some(old) = slot.take() {
            old.shutdown();
        }
        *slot = Some(TunnelMgr::new(bridge));
    }
    r.register("tunnel.open", handler(open_handler));
    r.register("tunnel.close", handler(close_handler));
}

pub fn is_tunnel_frame_kind(kind: &str) -> bool {
    FRAME_KINDS.contains(&kind)
}

pub fn enqueue_tunnel_frame(kind: &str, extras: &Args) {
    let Some(mgr) = active_manager() else {
        return;
    };
    mgr.enqueue_frame(
        kind,
        &raw_string(extras, "tunnel_id"),
        &raw_string(extras, "stream_id"),
        &raw_string(extras, "data_base64"),
        &raw_string(extras, "reason"),
    );
}

pub fn raw_string(args: &Args, key: &str) -> String {
    match args.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

pub struct TunnelMgr {
    bridge: Arc<dyn TunnelBridge>,
    tunnels: Mutex<HashMap<String, Arc<TunnelState>>>,
}

pub struct TunnelState {
    pub id: String,
    pub local_port: i64,
    pub local_host: String,
    pub dial_host: String,
    bridge: Arc<dyn TunnelBridge>,
    done: CancellationToken,
    inner: Mutex<StateInner>,
}

#[derive(Default)]
struct StateInner {
    streams: HashMap<String, Arc<TunnelStream>>,
    queues: HashMap<String, mpsc::Sender<Frame>>,
    closed: bool,
}

struct TunnelStream {
    id: String,
    tunnel_id: String,
    cancel: CancellationToken,
    writer: Arc<tokio::sync::Mutex<Option<OwnedWriteHalf>>>,
}

enum Frame {
    StreamOpen,
    Data(Vec<u8>),
    StreamClose(String),
}

impl TunnelMgr {
    pub fn new(bridge: Arc<dyn TunnelBridge>) -> Arc<TunnelMgr> {
        Arc::new(TunnelMgr {
            bridge,
            tunnels: Mutex::new(HashMap::new()),
        })
    }

    fn tunnels(&self) -> MutexGuard<'_, HashMap<String, Arc<TunnelState>>> {
        self.tunnels.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn shutdown(&self) {
        let states: Vec<Arc<TunnelState>> = self.tunnels().drain().map(|(_, t)| t).collect();
        for t in states {
            t.close_all("host shutdown");
        }
    }

    pub async fn open(
        &self,
        tunnel_id: &str,
        port: i64,
        host: &str,
    ) -> Result<Arc<TunnelState>, String> {
        self.open_with(tunnel_id, port, host, system_lookup).await
    }

    pub async fn open_with(
        &self,
        tunnel_id: &str,
        port: i64,
        host: &str,
        lookup: Lookup,
    ) -> Result<Arc<TunnelState>, String> {
        if tunnel_id.is_empty() {
            return Err("tunnel.open: tunnel_id is required".to_string());
        }
        if port <= 0 || port > 65535 {
            return Err(format!(
                "tunnel.open: local_port must be 1..65535 (got {port})"
            ));
        }
        let mut host = host.trim();
        if host.is_empty() {
            host = "127.0.0.1";
        }
        let (display, dial_ip) = validate_tunnel_target_host_with(host, lookup).await?;

        let mut tunnels = self.tunnels();
        if let Some(existing) = tunnels.get(tunnel_id) {
            if existing.local_port != port || existing.local_host != display {
                return Err(format!(
                    "tunnel.open: tunnel_id {tunnel_id} already open on a different port"
                ));
            }
            return Ok(existing.clone());
        }
        let t = Arc::new(TunnelState {
            id: tunnel_id.to_string(),
            local_port: port,
            local_host: display,
            dial_host: dial_ip,
            bridge: self.bridge.clone(),
            done: CancellationToken::new(),
            inner: Mutex::new(StateInner::default()),
        });
        tunnels.insert(tunnel_id.to_string(), t.clone());
        Ok(t)
    }

    pub fn get(&self, tunnel_id: &str) -> Option<Arc<TunnelState>> {
        self.tunnels().get(tunnel_id).cloned()
    }

    pub fn close_tunnel(&self, tunnel_id: &str, reason: &str) -> bool {
        let removed = self.tunnels().remove(tunnel_id);
        match removed {
            Some(t) => {
                t.close_all(reason);
                true
            }
            None => false,
        }
    }

    pub fn enqueue_frame(
        &self,
        kind: &str,
        tunnel_id: &str,
        stream_id: &str,
        data_b64: &str,
        reason: &str,
    ) {
        let Some(t) = self.get(tunnel_id) else {
            return;
        };
        if stream_id.is_empty() {
            return;
        }
        match kind {
            "tunnel.stream_open" => t.enqueue(stream_id, Frame::StreamOpen),
            "tunnel.data" => {
                let payload = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                    Ok(p) => p,
                    Err(err) => {
                        slog!(
                            "tunnel data bad base64 tunnel={tunnel_id} stream={stream_id}: {err}"
                        );
                        return;
                    }
                };
                if payload.len() > MAX_FRAME_PAYLOAD {
                    slog!(
                        "tunnel data oversize tunnel={tunnel_id} stream={stream_id} bytes={}",
                        payload.len()
                    );
                    let bridge = self.bridge.clone();
                    let (tid, sid) = (tunnel_id.to_string(), stream_id.to_string());
                    tokio::spawn(async move {
                        let _ = bridge
                            .send_tunnel_stream_close(&tid, &sid, "frame too large")
                            .await;
                    });
                    t.enqueue(stream_id, Frame::StreamClose("frame too large".to_string()));
                    return;
                }
                t.enqueue(stream_id, Frame::Data(payload));
            }
            "tunnel.stream_close" => {
                let reason = if reason.is_empty() {
                    "operator close"
                } else {
                    reason
                };
                t.enqueue(stream_id, Frame::StreamClose(reason.to_string()));
            }
            _ => {}
        }
    }
}

struct QueueGuard {
    state: Arc<TunnelState>,
    stream_id: String,
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.state.lock().queues.remove(&self.stream_id);
    }
}

impl TunnelState {
    fn lock(&self) -> MutexGuard<'_, StateInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn stream_count(&self) -> usize {
        self.lock().streams.len()
    }

    pub fn dial_target(&self) -> String {
        join_host_port(&self.dial_host, self.local_port)
    }

    fn enqueue(self: &Arc<Self>, stream_id: &str, f: Frame) {
        let tx = {
            let mut inner = self.lock();
            if inner.closed {
                return;
            }
            match inner.queues.get(stream_id) {
                Some(tx) => tx.clone(),
                None => {
                    let (tx, rx) = mpsc::channel(STREAM_QUEUE_DEPTH);
                    inner.queues.insert(stream_id.to_string(), tx.clone());
                    tokio::spawn(self.clone().consume(stream_id.to_string(), rx));
                    tx
                }
            }
        };
        match tx.try_send(f) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(f)) => self.send_blocking(stream_id, tx, f),
        }
    }

    fn send_blocking(&self, stream_id: &str, tx: mpsc::Sender<Frame>, f: Frame) {
        let multi = tokio::runtime::Handle::try_current()
            .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
            .unwrap_or(false);
        if !multi {
            slog!(
                "tunnel queue full tunnel={} stream={stream_id}; frame dropped",
                self.id
            );
            return;
        }
        let done = self.done.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                tokio::select! {
                    _ = tx.send(f) => {}
                    _ = done.cancelled() => {}
                }
            })
        });
    }

    async fn consume(self: Arc<Self>, stream_id: String, mut rx: mpsc::Receiver<Frame>) {
        let _guard = QueueGuard {
            state: self.clone(),
            stream_id: stream_id.clone(),
        };
        let mut s: Option<Arc<TunnelStream>> = None;
        loop {
            let f = tokio::select! {
                _ = self.done.cancelled() => {
                    if let Some(s) = &s {
                        s.close("tunnel closed");
                    }
                    return;
                }
                f = rx.recv() => f,
            };
            let Some(f) = f else {
                return;
            };
            match f {
                Frame::StreamOpen => {
                    if s.is_some() {
                        continue;
                    }
                    match self.dial_stream(&stream_id).await {
                        Ok(st) => s = Some(st),
                        Err(err) => {
                            slog!(
                                "tunnel dial failed tunnel={} stream={stream_id}: {err}",
                                self.id
                            );
                            let _ = self
                                .bridge
                                .send_tunnel_stream_close(
                                    &self.id,
                                    &stream_id,
                                    &format!("dial error: {err}"),
                                )
                                .await;
                        }
                    }
                }
                Frame::Data(payload) => {
                    let Some(st) = &s else {
                        continue;
                    };
                    if let Err(err) = st.write(&payload).await {
                        slog!(
                            "tunnel write failed tunnel={} stream={stream_id}: {err}",
                            self.id
                        );
                        let _ = self
                            .bridge
                            .send_tunnel_stream_close(
                                &self.id,
                                &stream_id,
                                &format!("write error: {err}"),
                            )
                            .await;
                        st.close("write error");
                        s = None;
                    }
                }
                Frame::StreamClose(reason) => {
                    if let Some(st) = &s {
                        st.close(&reason);
                    }
                    return;
                }
            }
        }
    }

    async fn dial_stream(self: &Arc<Self>, stream_id: &str) -> Result<Arc<TunnelStream>, String> {
        let target = {
            let inner = self.lock();
            if inner.closed {
                return Err(format!("tunnel {} is closed", self.id));
            }
            if inner.streams.contains_key(stream_id) {
                return Err(format!("stream_id {stream_id} already open"));
            }
            self.dial_target()
        };

        let conn = match tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(target.as_str()))
            .await
        {
            Ok(Ok(c)) => c,
            Ok(Err(err)) => return Err(format!("dial {target}: {}", go_dial_error(&target, &err))),
            Err(_) => return Err(format!("dial {target}: dial tcp {target}: i/o timeout")),
        };
        let (rd, wr) = conn.into_split();
        let stream = Arc::new(TunnelStream {
            id: stream_id.to_string(),
            tunnel_id: self.id.clone(),
            cancel: CancellationToken::new(),
            writer: Arc::new(tokio::sync::Mutex::new(Some(wr))),
        });

        {
            let mut inner = self.lock();
            if inner.closed {
                drop(inner);
                stream.cancel.cancel();
                return Err(format!("tunnel {} closed during dial", self.id));
            }
            inner.streams.insert(stream_id.to_string(), stream.clone());
        }

        tokio::spawn(self.clone().read_pump(rd, stream.clone()));
        slog!(
            "tunnel stream open tunnel={} stream={stream_id} local={target}",
            self.id
        );
        Ok(stream)
    }

    async fn read_pump(self: Arc<Self>, mut rd: OwnedReadHalf, s: Arc<TunnelStream>) {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let res = tokio::select! {
                _ = s.cancel.cancelled() => break,
                r = rd.read(&mut buf) => r,
            };
            let reason = match res {
                Ok(0) => "eof".to_string(),
                Ok(n) => {
                    if s.cancel.is_cancelled() {
                        break;
                    }
                    if let Err(err) = self
                        .bridge
                        .send_tunnel_data(&self.id, &s.id, &buf[..n])
                        .await
                    {
                        slog!(
                            "tunnel send data failed tunnel={} stream={}: {err}",
                            self.id,
                            s.id
                        );
                        break;
                    }
                    continue;
                }
                Err(err) => format!("read error: {}", go_io_error(&err)),
            };
            if !s.cancel.is_cancelled() {
                let _ = self
                    .bridge
                    .send_tunnel_stream_close(&self.id, &s.id, &reason)
                    .await;
            }
            slog!(
                "tunnel stream end tunnel={} stream={} reason={reason}",
                self.id,
                s.id
            );
            break;
        }
        {
            let mut inner = self.lock();
            if inner
                .streams
                .get(&s.id)
                .is_some_and(|existing| Arc::ptr_eq(existing, &s))
            {
                inner.streams.remove(&s.id);
            }
        }
        drop(rd);
        s.writer.lock().await.take();
    }

    pub fn close_all(&self, reason: &str) {
        let streams: Vec<Arc<TunnelStream>> = {
            let mut inner = self.lock();
            inner.closed = true;
            inner.streams.drain().map(|(_, s)| s).collect()
        };
        self.done.cancel();
        for s in streams {
            s.close(reason);
        }
        slog!("tunnel close tunnel={} reason={reason}", self.id);
    }
}

impl TunnelStream {
    async fn write(&self, payload: &[u8]) -> Result<(), String> {
        let write = async {
            let mut guard = self.writer.lock().await;
            match guard.as_mut() {
                Some(w) => w.write_all(payload).await.map_err(|e| go_io_error(&e)),
                None => Err("use of closed network connection".to_string()),
            }
        };
        match tokio::time::timeout(WRITE_DEADLINE, write).await {
            Ok(r) => r,
            Err(_) => Err("i/o timeout".to_string()),
        }
    }

    fn close(&self, reason: &str) {
        self.cancel.cancel();
        match self.writer.try_lock() {
            Ok(mut guard) => {
                guard.take();
            }
            Err(_) => {
                let writer = self.writer.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        writer.lock().await.take();
                    });
                }
            }
        }
        slog!(
            "tunnel stream close tunnel={} stream={} reason={reason}",
            self.tunnel_id,
            self.id
        );
    }
}

fn strip_os_error(text: &str) -> String {
    match text.rfind(" (os error ") {
        Some(i) if text.ends_with(')') => text[..i].to_string(),
        _ => text.to_string(),
    }
}

fn go_io_error(err: &std::io::Error) -> String {
    strip_os_error(&err.to_string())
}

fn go_dial_error(target: &str, err: &std::io::Error) -> String {
    format!("dial tcp {target}: connectex: {}", go_io_error(err))
}

pub fn join_host_port(host: &str, port: i64) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn system_lookup(host: String) -> BoxFuture<'static, Result<Vec<IpAddr>, String>> {
    Box::pin(async move {
        match tokio::net::lookup_host((host.as_str(), 0)).await {
            Ok(addrs) => {
                let mut out: Vec<IpAddr> = Vec::new();
                for a in addrs {
                    if !out.contains(&a.ip()) {
                        out.push(a.ip());
                    }
                }
                Ok(out)
            }
            Err(err) => {
                let text = match err.raw_os_error() {
                    Some(11001) | Some(11004) => "no such host".to_string(),
                    _ => go_io_error(&err),
                };
                Err(format!("lookup {host}: {text}"))
            }
        }
    })
}

pub fn split_host_port_ok(hostport: &str) -> bool {
    let Some(i) = hostport.rfind(':') else {
        return false;
    };
    let (j, k);
    if hostport.starts_with('[') {
        let Some(end) = hostport.find(']') else {
            return false;
        };
        if end + 1 == hostport.len() || end + 1 != i {
            return false;
        }
        j = 1;
        k = end + 1;
    } else {
        if hostport[..i].contains(':') {
            return false;
        }
        j = 0;
        k = 0;
    }
    if hostport[j..].contains('[') {
        return false;
    }
    if hostport[k..].contains(']') {
        return false;
    }
    true
}

pub async fn validate_tunnel_target_host(host: &str) -> Result<(String, String), String> {
    validate_tunnel_target_host_with(host, system_lookup).await
}

pub async fn validate_tunnel_target_host_with(
    host: &str,
    lookup: Lookup,
) -> Result<(String, String), String> {
    let host = host.trim();
    if host.is_empty() {
        return Ok(("127.0.0.1".to_string(), "127.0.0.1".to_string()));
    }
    if host.contains("://") || host.contains(['/', '\\', '?', '#', '@']) {
        return Err(format!(
            "tunnel.open: host must be a hostname or IP address (got {})",
            go_quote(host)
        ));
    }
    if split_host_port_ok(host) {
        return Err(format!(
            "tunnel.open: host must not include a port (got {})",
            go_quote(host)
        ));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_allowed_tunnel_target_ip(ip) {
            return Err(format!(
                "tunnel.open: host {} is not a local network address",
                go_quote(host)
            ));
        }
        return Ok((host.to_string(), host.to_string()));
    }
    if !is_reasonable_tunnel_hostname(host) {
        return Err(format!("tunnel.open: invalid host {}", go_quote(host)));
    }

    let addrs = match tokio::time::timeout(HOST_LOOKUP_TIME, lookup(host.to_string())).await {
        Ok(Ok(a)) => a,
        Ok(Err(err)) => {
            return Err(format!(
                "tunnel.open: resolve host {}: {err}",
                go_quote(host)
            ));
        }
        Err(_) => {
            return Err(format!(
                "tunnel.open: resolve host {}: lookup {host}: i/o timeout",
                go_quote(host)
            ));
        }
    };
    if addrs.is_empty() {
        return Err(format!(
            "tunnel.open: resolve host {}: no addresses",
            go_quote(host)
        ));
    }
    for ip in &addrs {
        if !is_allowed_tunnel_target_ip(*ip) {
            return Err(format!(
                "tunnel.open: host {} resolves outside the local network ({})",
                go_quote(host),
                canonical_ip(*ip)
            ));
        }
    }
    Ok((host.to_string(), canonical_ip(addrs[0]).to_string()))
}

pub fn is_reasonable_tunnel_hostname(host: &str) -> bool {
    if host.len() > 253 {
        return false;
    }
    host.chars()
        .all(|r| r == '.' || r == '-' || r == '_' || r.is_ascii_alphanumeric())
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        v4 => v4,
    }
}

fn v4_allowed(ip: Ipv4Addr) -> bool {
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    if ip.is_loopback() || ip.is_private() || ip.is_link_local() {
        return true;
    }
    let o = ip.octets();
    o[0] == 100 && o[1] & 0xc0 == 64
}

fn v6_allowed(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    let seg = ip.segments();
    ip.is_loopback() || seg[0] & 0xfe00 == 0xfc00 || seg[0] & 0xffc0 == 0xfe80
}

pub fn is_allowed_tunnel_target_ip(ip: IpAddr) -> bool {
    match canonical_ip(ip) {
        IpAddr::V4(v4) => v4_allowed(v4),
        IpAddr::V6(v6) => v6_allowed(v6),
    }
}

fn is_loopback_ip_text(host: &str) -> bool {
    host.parse::<IpAddr>()
        .map(|ip| canonical_ip(ip).is_loopback())
        .unwrap_or(false)
}

pub fn tunnel_target_description(host_arg: &str, port: i64) -> String {
    let mut host = host_arg.trim();
    if host.is_empty() {
        host = "127.0.0.1";
    }
    if host.eq_ignore_ascii_case("localhost") || is_loopback_ip_text(host) {
        return format!("a service running on this computer ({host}:{port})");
    }
    format!("a device on this computer's local network, {host}:{port} (for example a router or printer)")
}

async fn open_handler(ctx: Ctx, args: Args) -> CapResult {
    let tunnel_id = raw_string(&args, "tunnel_id");
    let port = args.get("local_port").and_then(Value::as_i64).unwrap_or(0);
    let host_arg = raw_string(&args, "host");

    let Some(mgr) = active_manager() else {
        return Err(HandlerError::plain(
            "tunnel.open: tunnel manager not registered",
        ));
    };

    let summary = format!(
        "Opens a network tunnel so the helper can reach {}. It stays open until you end the session or the helper closes it.",
        tunnel_target_description(&host_arg, port)
    );
    require_risk_consent(&ctx.cancel, "tunnel.open", &summary).await?;
    let t = mgr
        .open(&tunnel_id, port, &host_arg)
        .await
        .map_err(HandlerError::plain)?;
    slog!(
        "tunnel open tunnel={} local={}:{}",
        t.id,
        t.local_host,
        t.local_port
    );
    Ok(json!({
        "tunnel_id": t.id,
        "local_port": t.local_port,
        "local_host": t.local_host,
    }))
}

async fn close_handler(_ctx: Ctx, args: Args) -> CapResult {
    let tunnel_id = raw_string(&args, "tunnel_id");
    let mut reason = raw_string(&args, "reason");
    let Some(mgr) = active_manager() else {
        return Ok(json!({"closed": false}));
    };
    if reason.is_empty() {
        reason = "operator close".to_string();
    }
    Ok(json!({"closed": mgr.close_tunnel(&tunnel_id, &reason)}))
}

#[cfg(test)]
mod tests {
    use super::super::consent::testing::SessionGuard;
    use super::*;
    use crate::consent::Prompt;
    use std::time::Instant;
    use tokio::net::TcpListener;

    #[derive(Default)]
    struct FakeBridge {
        data: Mutex<Vec<(String, String, Vec<u8>)>>,
        closes: Mutex<Vec<String>>,
    }

    impl TunnelBridge for FakeBridge {
        fn send_tunnel_data<'a>(
            &'a self,
            tunnel_id: &'a str,
            stream_id: &'a str,
            data: &'a [u8],
        ) -> BoxFuture<'a, Result<(), String>> {
            self.data.lock().unwrap().push((
                tunnel_id.to_string(),
                stream_id.to_string(),
                data.to_vec(),
            ));
            Box::pin(async { Ok(()) })
        }

        fn send_tunnel_stream_close<'a>(
            &'a self,
            tunnel_id: &'a str,
            stream_id: &'a str,
            reason: &'a str,
        ) -> BoxFuture<'a, Result<(), String>> {
            self.closes
                .lock()
                .unwrap()
                .push(format!("{tunnel_id}:{stream_id}:{reason}"));
            Box::pin(async { Ok(()) })
        }

        fn send_tunnel_close<'a>(
            &'a self,
            tunnel_id: &'a str,
            reason: &'a str,
        ) -> BoxFuture<'a, Result<(), String>> {
            self.closes
                .lock()
                .unwrap()
                .push(format!("{tunnel_id}:TUNNEL:{reason}"));
            Box::pin(async { Ok(()) })
        }
    }

    impl FakeBridge {
        fn stream_bytes(&self, tunnel_id: &str, stream_id: &str) -> Vec<u8> {
            let mut out = Vec::new();
            for (t, s, p) in self.data.lock().unwrap().iter() {
                if t == tunnel_id && s == stream_id {
                    out.extend_from_slice(p);
                }
            }
            out
        }

        fn close_reasons(&self) -> Vec<String> {
            self.closes.lock().unwrap().clone()
        }
    }

    fn fake() -> (Arc<FakeBridge>, Arc<TunnelMgr>) {
        let bridge = Arc::new(FakeBridge::default());
        let mgr = TunnelMgr::new(bridge.clone());
        (bridge, mgr)
    }

    async fn wait_echo(b: &FakeBridge, tunnel_id: &str, stream_id: &str, want: usize) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let got = b.stream_bytes(tunnel_id, stream_id);
            if got.len() >= want {
                return got;
            }
            if Instant::now() > deadline {
                panic!(
                    "only {} of {want} bytes echoed back for {tunnel_id}/{stream_id}",
                    got.len()
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn open_stream(mgr: &TunnelMgr, tunnel_id: &str, stream_id: &str) {
        mgr.enqueue_frame("tunnel.stream_open", tunnel_id, stream_id, "", "");
    }

    fn write_ordered(mgr: &TunnelMgr, tunnel_id: &str, stream_id: &str, payload: &[u8]) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
        mgr.enqueue_frame("tunnel.data", tunnel_id, stream_id, &b64, "");
    }

    async fn start_echo_server() -> (i64, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port() as i64;
        let handle = tokio::spawn(async move {
            while let Ok((mut c, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = c.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        (port, handle)
    }

    async fn start_http_server() -> (i64, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port() as i64;
        let handle = tokio::spawn(async move {
            while let Ok((mut c, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match c.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let _ = c
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: 8\r\n\r\nfritz ok")
                        .await;
                });
            }
        });
        (port, handle)
    }

    async fn wait_stream_count(t: &TunnelState, want: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let n = t.stream_count();
            if n == want {
                return;
            }
            if Instant::now() > deadline {
                panic!("stream count = {n}, want {want}");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn prompt(answer: bool) -> Prompt {
        Arc::new(move |_, _| Box::pin(async move { Ok(answer) }))
    }

    #[test]
    fn frame_kinds_bypass_the_dispatcher() {
        for kind in ["tunnel.stream_open", "tunnel.data", "tunnel.stream_close"] {
            assert!(is_tunnel_frame_kind(kind), "{kind}");
        }
        for kind in ["tunnel.open", "tunnel.close", "sys.info", ""] {
            assert!(!is_tunnel_frame_kind(kind), "{kind}");
        }
    }

    #[test]
    fn raw_string_ignores_missing_and_non_string_values() {
        let args = json!({"a": "x", "b": 3}).as_object().cloned().unwrap();
        assert_eq!(raw_string(&args, "a"), "x");
        assert_eq!(raw_string(&args, "b"), "");
        assert_eq!(raw_string(&args, "c"), "");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manager_opens_and_forwards_bytes() {
        let (port, server) = start_echo_server().await;
        let (bridge, mgr) = fake();
        mgr.open("tn1", port, "127.0.0.1").await.unwrap();
        open_stream(&mgr, "tn1", "s1");
        let payload = b"hello tunnel\n";
        write_ordered(&mgr, "tn1", "s1", payload);
        let got = wait_echo(&bridge, "tn1", "s1", payload.len()).await;
        assert_eq!(got, payload);
        mgr.shutdown();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manager_preserves_data_order() {
        let (port, server) = start_echo_server().await;
        let (bridge, mgr) = fake();
        mgr.open("tn1", port, "127.0.0.1").await.unwrap();
        open_stream(&mgr, "tn1", "s1");
        let mut want = Vec::new();
        for i in 0..200 {
            let chunk = format!("{i:06}");
            want.extend_from_slice(chunk.as_bytes());
            write_ordered(&mgr, "tn1", "s1", chunk.as_bytes());
        }
        let got = wait_echo(&bridge, "tn1", "s1", want.len()).await;
        assert_eq!(&got[..want.len()], &want[..], "frames arrived out of order");
        mgr.shutdown();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_queue_backpressures_without_losing_frames() {
        let (port, server) = start_echo_server().await;
        let (bridge, mgr) = fake();
        mgr.open("tn1", port, "127.0.0.1").await.unwrap();
        open_stream(&mgr, "tn1", "s1");
        let mut want = Vec::new();
        for i in 0..(STREAM_QUEUE_DEPTH * 3) {
            let chunk = format!("{i:08}");
            want.extend_from_slice(chunk.as_bytes());
            write_ordered(&mgr, "tn1", "s1", chunk.as_bytes());
        }
        let got = wait_echo(&bridge, "tn1", "s1", want.len()).await;
        assert_eq!(&got[..want.len()], &want[..]);
        mgr.shutdown();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manager_forwards_http_response() {
        let (port, server) = start_http_server().await;
        let (bridge, mgr) = fake();
        mgr.open("tn1", port, "127.0.0.1").await.unwrap();
        open_stream(&mgr, "tn1", "s1");
        write_ordered(
            &mgr,
            "tn1",
            "s1",
            b"GET /login_sid.lua HTTP/1.1\r\nHost: fritz.repeater\r\nConnection: close\r\n\r\n",
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let got = bridge.stream_bytes("tn1", "s1");
            if got.windows(8).any(|w| w == b"fritz ok") {
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "HTTP response did not arrive; got {:?}",
                    String::from_utf8_lossy(&got)
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while !bridge.close_reasons().contains(&"tn1:s1:eof".to_string()) {
            assert!(Instant::now() < deadline, "{:?}", bridge.close_reasons());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        mgr.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn manager_rejects_non_loopback() {
        let (_, mgr) = fake();
        assert!(mgr.open("tn1", 5555, "8.8.8.8").await.is_err());
    }

    #[tokio::test]
    async fn manager_accepts_private_target() {
        let (_, mgr) = fake();
        let t = mgr.open("tn1", 5555, "192.168.2.1").await.unwrap();
        assert_eq!(t.local_host, "192.168.2.1");
        assert_eq!(t.dial_host, "192.168.2.1");
    }

    fn fritz_lookup(host: String) -> BoxFuture<'static, Result<Vec<IpAddr>, String>> {
        Box::pin(async move {
            assert_eq!(host, "fritz.repeater");
            Ok(vec!["192.168.2.103".parse().unwrap()])
        })
    }

    fn public_lookup(_: String) -> BoxFuture<'static, Result<Vec<IpAddr>, String>> {
        Box::pin(async { Ok(vec!["8.8.8.8".parse().unwrap()]) })
    }

    #[tokio::test]
    async fn manager_pins_resolved_hostname() {
        let (_, mgr) = fake();
        let t = mgr
            .open_with("tn1", 5555, "fritz.repeater", fritz_lookup)
            .await
            .unwrap();
        assert_eq!(t.local_host, "fritz.repeater");
        assert_eq!(t.dial_host, "192.168.2.103");
    }

    #[tokio::test]
    async fn manager_rejects_hostname_resolving_public() {
        let (_, mgr) = fake();
        let err = mgr
            .open_with("tn1", 5555, "example.com", public_lookup)
            .await
            .err()
            .expect("expected an error");
        assert_eq!(
            err,
            "tunnel.open: host \"example.com\" resolves outside the local network (8.8.8.8)"
        );
    }

    #[tokio::test]
    async fn manager_rejects_bad_port() {
        let (_, mgr) = fake();
        assert_eq!(
            mgr.open("tn1", 0, "127.0.0.1")
                .await
                .err()
                .expect("expected an error"),
            "tunnel.open: local_port must be 1..65535 (got 0)"
        );
        assert!(mgr.open("tn1", 70000, "127.0.0.1").await.is_err());
        assert_eq!(
            mgr.open("", 80, "127.0.0.1")
                .await
                .err()
                .expect("expected an error"),
            "tunnel.open: tunnel_id is required"
        );
    }

    #[tokio::test]
    async fn host_validation_messages_match_go() {
        let cases = [
            (
                "http://x",
                "tunnel.open: host must be a hostname or IP address (got \"http://x\")",
            ),
            (
                "a/b",
                "tunnel.open: host must be a hostname or IP address (got \"a/b\")",
            ),
            (
                "user@x",
                "tunnel.open: host must be a hostname or IP address (got \"user@x\")",
            ),
            (
                "fritz.box:80",
                "tunnel.open: host must not include a port (got \"fritz.box:80\")",
            ),
            (
                "[::1]:80",
                "tunnel.open: host must not include a port (got \"[::1]:80\")",
            ),
            (
                "8.8.8.8",
                "tunnel.open: host \"8.8.8.8\" is not a local network address",
            ),
            ("[::1]", "tunnel.open: invalid host \"[::1]\""),
            ("bad host", "tunnel.open: invalid host \"bad host\""),
        ];
        for (host, want) in cases {
            let err = validate_tunnel_target_host(host).await.unwrap_err();
            assert_eq!(err, want, "{host}");
        }
        assert_eq!(
            validate_tunnel_target_host("::1").await.unwrap(),
            ("::1".to_string(), "::1".to_string())
        );
        assert_eq!(
            validate_tunnel_target_host(" ").await.unwrap(),
            ("127.0.0.1".to_string(), "127.0.0.1".to_string())
        );
    }

    #[test]
    fn split_host_port_matches_go() {
        for ok in ["a:80", "a:", "[::1]:80", "1.2.3.4:5"] {
            assert!(split_host_port_ok(ok), "{ok}");
        }
        for bad in ["a", "::1", "[::1]", "a:b:c", "[a]b:1", "[[a]]:1"] {
            assert!(!split_host_port_ok(bad), "{bad}");
        }
    }

    #[test]
    fn allowed_tunnel_target_ips() {
        let allow = [
            "127.0.0.1",
            "::1",
            "192.168.1.1",
            "10.0.0.5",
            "172.16.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.254",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ];
        let deny = [
            "8.8.8.8",
            "1.1.1.1",
            "100.128.0.1",
            "100.63.255.255",
            "0.0.0.0",
            "224.0.0.1",
            "::",
            "ff02::1",
            "2001:4860:4860::8888",
        ];
        for s in allow {
            assert!(is_allowed_tunnel_target_ip(s.parse().unwrap()), "{s}");
        }
        for s in deny {
            assert!(!is_allowed_tunnel_target_ip(s.parse().unwrap()), "{s}");
        }
    }

    #[test]
    fn target_description() {
        assert!(tunnel_target_description("", 8080).contains("this computer"));
        assert!(tunnel_target_description("127.0.0.1", 80).contains("this computer"));
        assert!(tunnel_target_description("LocalHost", 80).contains("this computer"));
        assert_eq!(
            tunnel_target_description("192.168.1.1", 80),
            "a device on this computer's local network, 192.168.1.1:80 (for example a router or printer)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversize_data_closes_stream() {
        let (port, server) = start_echo_server().await;
        let (bridge, mgr) = fake();
        mgr.open("tn1", port, "127.0.0.1").await.unwrap();
        open_stream(&mgr, "tn1", "s1");
        write_ordered(&mgr, "tn1", "s1", &vec![0u8; MAX_FRAME_PAYLOAD + 1]);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !bridge
            .close_reasons()
            .iter()
            .any(|c| c.contains("frame too large"))
        {
            assert!(Instant::now() < deadline, "{:?}", bridge.close_reasons());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        mgr.shutdown();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_base64_does_not_kill_stream() {
        let (port, server) = start_echo_server().await;
        let (bridge, mgr) = fake();
        mgr.open("tn1", port, "127.0.0.1").await.unwrap();
        open_stream(&mgr, "tn1", "s1");
        mgr.enqueue_frame("tunnel.data", "tn1", "s1", "$$$not base64!!!", "");
        write_ordered(&mgr, "tn1", "s1", b"still alive");
        let got = wait_echo(&bridge, "tn1", "s1", 11).await;
        assert_eq!(got, b"still alive");
        mgr.shutdown();
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dial_failure_reports_stream_close() {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port() as i64;
        drop(probe);
        let (bridge, mgr) = fake();
        mgr.open("tn1", port, "127.0.0.1").await.unwrap();
        open_stream(&mgr, "tn1", "s1");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let closes = bridge.close_reasons();
            if let Some(c) = closes.first() {
                let want = format!("tn1:s1:dial error: dial 127.0.0.1:{port}: dial tcp 127.0.0.1:{port}: connectex: ");
                assert!(c.starts_with(&want), "{c}");
                assert!(!c.contains("os error"), "{c}");
                break;
            }
            assert!(Instant::now() < deadline, "no dial error reported");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        mgr.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_handler_requires_consent() {
        let _guard = SessionGuard::with_prompt(prompt(false));
        let (_, mgr) = fake();
        let old = set_manager(Some(mgr));
        let err = open_handler(
            Ctx::default(),
            args(json!({"tunnel_id": "tn1", "local_port": 8080})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.message, "the host declined this request");
        if let Some(m) = set_manager(old) {
            m.shutdown();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_through_global_manager() {
        let _guard = SessionGuard::with_prompt(prompt(true));
        let (port, server) = start_echo_server().await;
        let bridge = Arc::new(FakeBridge::default());
        let old = set_manager(Some(TunnelMgr::new(bridge.clone())));

        let res = open_handler(
            Ctx::default(),
            args(json!({"tunnel_id": "tn1", "local_port": port})),
        )
        .await
        .unwrap();
        assert_eq!(
            res,
            json!({"tunnel_id": "tn1", "local_port": port, "local_host": "127.0.0.1"})
        );
        enqueue_tunnel_frame(
            "tunnel.stream_open",
            &args(json!({"tunnel_id": "tn1", "stream_id": "s1"})),
        );
        let payload = b"round trip bytes";
        enqueue_tunnel_frame(
            "tunnel.data",
            &args(json!({
                "tunnel_id": "tn1",
                "stream_id": "s1",
                "data_base64": base64::engine::general_purpose::STANDARD.encode(payload),
            })),
        );
        let got = wait_echo(&bridge, "tn1", "s1", payload.len()).await;
        assert_eq!(got, payload);

        enqueue_tunnel_frame(
            "tunnel.stream_close",
            &args(json!({"tunnel_id": "tn1", "stream_id": "s1"})),
        );
        let res = close_handler(Ctx::default(), args(json!({"tunnel_id": "tn1"})))
            .await
            .unwrap();
        assert_eq!(res, json!({"closed": true}));
        let res = close_handler(Ctx::default(), args(json!({"tunnel_id": "tn1"})))
            .await
            .unwrap();
        assert_eq!(res, json!({"closed": false}));

        if let Some(m) = set_manager(old) {
            m.shutdown();
        }
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_closes_active_streams() {
        let (port, server) = start_echo_server().await;
        let (_, mgr) = fake();
        let t = mgr.open("tn1", port, "127.0.0.1").await.unwrap();
        open_stream(&mgr, "tn1", "s1");
        wait_stream_count(&t, 1).await;
        mgr.shutdown();
        assert!(mgr.get("tn1").is_none());
        assert_eq!(t.stream_count(), 0);
        server.abort();
    }

    #[tokio::test]
    async fn open_with_existing_tunnel_id_returns_existing() {
        let (_, mgr) = fake();
        let first = mgr.open("tn1", 8080, "127.0.0.1").await.unwrap();
        let second = mgr.open("tn1", 8080, "127.0.0.1").await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            mgr.open("tn1", 9090, "127.0.0.1")
                .await
                .err()
                .expect("expected an error"),
            "tunnel.open: tunnel_id tn1 already open on a different port"
        );
        mgr.shutdown();
    }
}

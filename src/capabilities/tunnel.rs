use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use super::consent::require_risk_consent;
pub use super::tunnel_host::HOST_LOOKUP_TIME;
pub use super::tunnel_host::Lookup;
pub use super::tunnel_host::check_tunnel_host_text;
pub use super::tunnel_host::is_allowed_tunnel_target_ip;
pub use super::tunnel_host::is_reasonable_tunnel_hostname;
pub use super::tunnel_host::join_host_port;
pub use super::tunnel_host::split_host_port_ok;
pub use super::tunnel_host::tunnel_target_description;
pub use super::tunnel_host::validate_tunnel_target_host;
pub use super::tunnel_host::validate_tunnel_target_host_with;
pub use super::tunnel_mgr::TunnelMgr;
pub use super::tunnel_state::DIAL_TIMEOUT;
pub use super::tunnel_state::MAX_FRAME_PAYLOAD;
pub use super::tunnel_state::READ_CHUNK;
pub use super::tunnel_state::STREAM_QUEUE_DEPTH;
pub use super::tunnel_state::TunnelState;
pub use super::tunnel_state::WRITE_DEADLINE;
use crate::dispatch::{Args, CapResult, Ctx, HandlerError, Router, handler};
use crate::relay::Bridge;

pub const FRAME_KINDS: [&str; 3] = ["tunnel.stream_open", "tunnel.data", "tunnel.stream_close"];

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
    super::register_specs(
        r,
        include_str!("specs/tunnel.json"),
        vec![
            ("tunnel.open", handler(open_handler)),
            ("tunnel.close", handler(close_handler)),
        ],
    );
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

async fn open_handler(ctx: Ctx, args: Args) -> CapResult {
    let tunnel_id = raw_string(&args, "tunnel_id");
    let port = args.get("local_port").and_then(Value::as_i64).unwrap_or(0);
    let host_arg = raw_string(&args, "host");

    let Some(mgr) = active_manager() else {
        return Err(HandlerError::plain(
            "tunnel.open: tunnel manager not registered",
        ));
    };
    check_tunnel_host_text(&host_arg).map_err(HandlerError::plain)?;

    let summary = format!(
        "Opens a network tunnel so the helper can reach {}. It stays open until you end the session or the helper closes it.",
        tunnel_target_description(&host_arg, port)
    );
    require_risk_consent(&ctx, "tunnel.open", &summary).await?;
    let t = mgr
        .open(&tunnel_id, port, &host_arg)
        .await
        .map_err(HandlerError::plain)?;
    tracing::info!(
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
    use crate::capabilities::Duration;
    use crate::consent::Prompt;
    use base64::Engine;
    use std::net::IpAddr;
    use std::time::Instant;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
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
        let mgr = TunnelMgr::new(Arc::<FakeBridge>::clone(&bridge));
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
        let port = i64::from(listener.local_addr().unwrap().port());
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
        let port = i64::from(listener.local_addr().unwrap().port());
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
        serde_json::from_value(v).unwrap_or_default()
    }

    fn prompt(answer: bool) -> Prompt {
        Arc::new(move |_, _| {
            Box::pin(async move {
                Ok(if answer {
                    crate::consent::Answer::ForGrant
                } else {
                    crate::consent::Answer::No
                })
            })
        })
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
        let port = i64::from(probe.local_addr().unwrap().port());
        drop(probe);
        let (bridge, mgr) = fake();
        mgr.open("tn1", port, "127.0.0.1").await.unwrap();
        open_stream(&mgr, "tn1", "s1");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let closes = bridge.close_reasons();
            if let Some(c) = closes.first() {
                let want = format!(
                    "tn1:s1:dial error: dial 127.0.0.1:{port}: dial tcp 127.0.0.1:{port}: connectex: "
                );
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
    async fn open_handler_refuses_a_bad_host_before_asking() {
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&asked);
        let _guard = SessionGuard::with_prompt(Arc::new(move |_, _| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(crate::consent::Answer::ForGrant) })
        }));
        let (_, mgr) = fake();
        let old = set_manager(Some(mgr));
        for host in [
            "a".repeat(10_000),
            "printer:80".to_string(),
            "8.8.8.8".to_string(),
        ] {
            let err = open_handler(
                Ctx::default(),
                args(json!({"tunnel_id": "tn1", "local_port": 8080, "host": host})),
            )
            .await
            .unwrap_err();
            assert!(err.message.starts_with("tunnel.open: "), "{}", err.message);
        }
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 0);
        if let Some(m) = set_manager(old) {
            m.shutdown();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn end_to_end_through_global_manager() {
        let _guard = SessionGuard::with_prompt(prompt(true));
        let (port, server) = start_echo_server().await;
        let bridge = Arc::new(FakeBridge::default());
        let old = set_manager(Some(TunnelMgr::new(Arc::<FakeBridge>::clone(&bridge))));

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

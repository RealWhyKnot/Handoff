use std::time::Duration;

use tokio_util::sync::CancellationToken;

pub use super::tunnel_args::TunnelOptions;
pub use super::tunnel_args::bind_local_listener;
pub use super::tunnel_args::clean_http_host;
pub use super::tunnel_args::is_tunnel_url;
pub use super::tunnel_args::parse_tunnel_args;
pub use super::tunnel_args::parse_tunnel_args_with;
pub use super::tunnel_args::tunnel_ws_url;
pub use super::tunnel_client::new_stream_id;
pub use super::tunnel_http::HTTP_HEADER_REWRITE_LIMIT;
pub use super::tunnel_http::default_tunnel_http_host;
pub use super::tunnel_http::rewrite_http_host_header;
pub use super::tunnel_session::READ_LIMIT;
pub use super::tunnel_session::READY_TIMEOUT;
pub use super::tunnel_session::SESSION_DIAL_TIMEOUT;
pub use super::tunnel_session::run_sessions;
pub use super::tunnel_wire::TunnelReady;

pub const DEFAULT_LOCAL_PORT: u16 = 47800;
pub const PORT_SCAN_RANGE: u32 = 20;

pub const USAGE: &str =
    "usage: handoff tunnel <connect-token> [--local-port PORT] [--relay URL] [--http-host HOST]";

fn short_sid(t: &str) -> &str {
    t.get(..11).unwrap_or(t)
}

pub fn tunnel(args: &[String]) -> i32 {
    let opts = match parse_tunnel_args(args) {
        Ok(o) => o,
        Err(err) => {
            crate::errln!("handoff tunnel: {err}");
            crate::errln!("{USAGE}");
            return 2;
        }
    };

    tracing::info!(
        "tunnel-client start token={} local={} relay={}",
        short_sid(&opts.token),
        opts.local_port,
        opts.relay
    );

    let ws_url = match tunnel_ws_url(&opts.relay, &opts.token) {
        Ok(u) => u,
        Err(err) => {
            crate::errln!("handoff tunnel: {err}");
            return 1;
        }
    };

    let listener = match bind_local_listener(opts.local_port, opts.port_explicit) {
        Ok(l) => l,
        Err(err) => {
            crate::errln!("could not bind local port: {err}");
            if !opts.port_explicit {
                crate::errln!(
                    "tried ports {}-{}; pass --local-port to choose another",
                    opts.local_port,
                    u32::from(opts.local_port) + PORT_SCAN_RANGE - 1
                );
            }
            return 1;
        }
    };
    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    if bound_port != opts.local_port {
        crate::outln!(
            "port {} was busy, using {} instead",
            opts.local_port,
            bound_port
        );
    }

    crate::outln!("connecting to relay: {}", opts.relay);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            crate::errln!("{err}");
            return 1;
        }
    };
    let cancel = CancellationToken::new();
    let result = runtime.block_on(async {
        let on_signal = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                crate::outln!(
                    "
closing tunnel..."
                );
                tracing::info!("tunnel-client shutdown requested");
                on_signal.cancel();
            }
        });
        run_sessions(opts, ws_url, listener, bound_port, cancel).await
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
    match result {
        Ok(()) => 0,
        Err(err) => {
            tracing::info!("tunnel session ended: {err}");
            crate::errln!("handoff tunnel: {err}");
            1
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::cmd::tunnel_wire::WireFrame;
    use crate::zwire;
    use crate::zwire::Packer;
    use crate::zwire::Unpacker;
    use base64::Engine;
    use futures_util::SinkExt;
    use futures_util::StreamExt;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{
        Callback, ErrorResponse, Request, Response,
    };
    use tokio_tungstenite::tungstenite::http::HeaderValue;

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
        let counter = Arc::clone(&dials);
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
        let record = Arc::clone(&seen);
        let (url, _, server) = ws_server(move |_, mut ws| {
            let record = Arc::clone(&record);
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

    struct ZstdEcho;

    impl Callback for ZstdEcho {
        fn on_request(self, req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
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
        }
    }

    #[tokio::test]
    async fn a_relay_that_echoes_zstd_gets_compressed_frames_both_ways() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/tunnel/tk", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let record = Arc::clone(&seen);
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(tcp, ZstdEcho)
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

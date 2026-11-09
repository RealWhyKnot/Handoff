use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use super::relay_bridge::Bridge;
pub use super::relay_conn::http_to_ws;
pub use super::relay_proto::Command;
pub use super::relay_proto::Delivery;
pub use super::relay_proto::KeepaliveTiming;
pub use super::relay_proto::RecvError;
use crate::util;

pub const READ_LIMIT: usize = 8 << 20;
pub const PING_INTERVAL: Duration = Duration::from_secs(15);
pub const PING_TIMEOUT: Duration = Duration::from_secs(30);
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
pub const PENDING_RESULTS_CAP: usize = 32;
pub const PRESENCE_KIND: &str = "control.presence";
pub const UNREADABLE_FRAME: &str = "This computer couldn't read that request, so it didn't run it.";
pub const UNREADABLE_NO_KIND: &str = "The request didn't say which command to run, so nothing ran.";

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

#[cfg(test)]
mod tests {

    use super::*;
    use crate::dispatch::Clamp;
    use crate::dispatch::Outcome;
    use crate::dispatch::Spec;
    use crate::dispatch::{Param, ParamType};
    use crate::relay_bridge::Inner;
    use crate::relay_proto::salvage_id;
    use futures_util::SinkExt;
    use futures_util::StreamExt;
    use serde_json::Value;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;
    use tokio::task::JoinHandle;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{
        Callback, ErrorResponse, Request, Response,
    };
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_util::sync::CancellationToken;

    struct TestServer {
        url: String,
        listener: TcpListener,
    }

    async fn server() -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        TestServer { url, listener }
    }

    struct TokenCapture(Arc<Mutex<String>>);

    impl Callback for TokenCapture {
        fn on_request(self, req: &Request, resp: Response) -> Result<Response, ErrorResponse> {
            if let Some(v) = req.headers().get("X-Write-Token") {
                *self.0.lock().unwrap() = v.to_str().unwrap().to_string();
            }
            Ok(resp)
        }
    }

    async fn accept(listener: &TcpListener) -> (WsStream2, String) {
        let (tcp, _) = listener.accept().await.unwrap();
        let token = Arc::new(Mutex::new(String::new()));
        let ws = tokio_tungstenite::accept_hdr_async(tcp, TokenCapture(Arc::clone(&token)))
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
            params: vec![
                Param::new("limit", ParamType::Int)
                    .default_value(json!(300))
                    .range(1, 5000),
            ],
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

use super::new::{ReconnectBackoff, sleep_cancel};
use super::tunnel_args::TunnelOptions;
use super::tunnel_client::TunnelClient;
use super::tunnel_http::default_tunnel_http_host;
use super::tunnel_wire::TunnelReady;
use super::tunnel_wire::WireFrame;
use crate::zwire::{self, Packer, Unpacker};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{FutureExt, StreamExt};
use std::net::TcpListener;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

pub const READY_TIMEOUT: Duration = Duration::from_secs(15);

pub const SESSION_DIAL_TIMEOUT: Duration = Duration::from_secs(15);

pub const READ_LIMIT: usize = 8 << 20;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) type WsSink = SplitSink<WsStream, Message>;

pub(super) type WsSource = SplitStream<WsStream>;

pub(super) enum LoopEnd {
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
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set the local listener to non-blocking: {e}"))?;
    let listener = Arc::new(
        tokio::net::TcpListener::from_std(listener)
            .map_err(|e| format!("register the local listener: {e}"))?,
    );
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
                tracing::info!("tunnel session panic: {text}");
                (false, Some(format!("internal error: {text}")))
            }
        };
        if done || cancel.is_cancelled() {
            return Ok(());
        }
        if let Some(err) = err {
            tracing::info!("tunnel session ended: {err}");
            crate::errln!("tunnel connection lost: {err}");
        }
        crate::outln!("reconnecting...");
        if !sleep_cancel(&cancel, backoff.next_delay()).await {
            return Ok(());
        }
    }
}

pub(super) fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
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
    crate::outln!(
        "tunnel ready: forwarding 127.0.0.1:{} -> host {}:{}",
        bound_port,
        ready.host_addr,
        ready.host_port
    );
    if !*announced {
        if !http_host.is_empty() {
            crate::outln!("HTTP Host headers will use {http_host}");
        }
        crate::outln!("open http://127.0.0.1:{bound_port}/ in your browser");
        crate::outln!("press Ctrl+C to close the tunnel.");
        *announced = true;
    }

    let client = Arc::new(TunnelClient::new(sink, zip, http_host));
    let session = cancel.child_token();
    tokio::spawn(Arc::clone(&client).accept_loop(session.clone(), Arc::clone(listener)));

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

pub(super) fn inflate(msg: Message, unzip: &mut Option<Unpacker>) -> Result<Message, String> {
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

pub(super) fn close_text(frame: Option<(u16, String)>) -> String {
    match frame {
        Some((code, reason)) => {
            format!("received close frame: status = {code} and reason = {reason:?}")
        }
        None => "EOF".to_string(),
    }
}

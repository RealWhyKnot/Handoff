use super::relay::READ_LIMIT;
use super::relay_proto::Incoming;
use super::relay_proto::KeepaliveTiming;
use crate::util;
use crate::zwire::{self, Packer, Unpacker};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

type WsSink = SplitSink<WsStream, Message>;

type WsSource = SplitStream<WsStream>;

pub(super) struct Writer {
    pub(super) sink: WsSink,
    pub(super) zip: Option<Packer>,
}

impl Writer {
    pub(super) async fn send_text(&mut self, text: String) -> Result<(), String> {
        let msg = match &mut self.zip {
            Some(zip) => Message::binary(
                zip.pack(text.as_bytes())
                    .map_err(|e| format!("compress a relay frame: {e}"))?,
            ),
            None => Message::text(text),
        };
        self.sink
            .send(msg)
            .await
            .map_err(|e| format!("send a relay frame: {e}"))
    }
}

pub(super) struct Conn {
    pub(super) sink: tokio::sync::Mutex<Writer>,
    pub(super) rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Incoming>>,
    pub(super) stale: AtomicBool,
    reader: AbortHandle,
    keepalive: Mutex<Option<JoinHandle<()>>>,
}

impl Conn {
    pub(super) fn stop_keepalive(&self) {
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

pub fn http_to_ws(base: &str) -> Result<String, String> {
    let mut u = url::Url::parse(base).map_err(|e| format!("parse the relay address: {e}"))?;
    let scheme = match u.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => return Err(format!("unsupported scheme {other:?} in {base:?}")),
    };
    u.set_scheme(scheme)
        .map_err(|()| format!("unsupported scheme in {base:?}"))?;
    Ok(u.to_string())
}

pub(super) async fn dial_conn(
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
    let handle = tokio::spawn(keepalive(Arc::clone(&conn), pong_rx, timing));
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
                    .map_err(|e| format!("decompress a relay frame: {e}"))
                    .and_then(|b| {
                        String::from_utf8(b).map_err(|e| format!("decode a relay frame: {e}"))
                    });
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
                .map_err(|_send_failed| false)?;
            pongs
                .wait_for(|n| *n > before)
                .await
                .map(|_| ())
                .map_err(|_closed| false)
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

pub(super) async fn close_conn(conn: &Conn, reason: &'static str) {
    let closing = async {
        let mut writer = conn.sink.lock().await;
        let _ = writer
            .sink
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: reason.into(),
            })))
            .await;
        drop(writer);
    };
    let _ = tokio::time::timeout(Duration::from_secs(1), closing).await;
    conn.reader.abort();
}

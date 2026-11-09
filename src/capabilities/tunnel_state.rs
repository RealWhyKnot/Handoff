use super::tunnel::TunnelBridge;
use super::tunnel_host::join_host_port;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const READ_CHUNK: usize = 16 * 1024;

pub const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;

const _: () = assert!(MAX_FRAME_PAYLOAD >= 64 * 1024);

pub const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

pub const STREAM_QUEUE_DEPTH: usize = 1024;

pub const WRITE_DEADLINE: Duration = Duration::from_secs(60);

pub struct TunnelState {
    pub id: String,
    pub local_port: i64,
    pub local_host: String,
    pub dial_host: String,
    pub(super) bridge: Arc<dyn TunnelBridge>,
    pub(super) done: CancellationToken,
    pub(super) inner: Mutex<StateInner>,
}

#[derive(Default)]
pub(super) struct StateInner {
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

pub(super) enum Frame {
    StreamOpen,
    Data(Vec<u8>),
    StreamClose(String),
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

    pub(super) fn enqueue(self: &Arc<Self>, stream_id: &str, f: Frame) {
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
                    drop(inner);
                    tokio::spawn(Arc::clone(self).consume(stream_id.to_string(), rx));
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
            tracing::info!(
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
            state: Arc::clone(&self),
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
                            tracing::info!(
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
                        tracing::info!(
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
            drop(inner);
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
            inner
                .streams
                .insert(stream_id.to_string(), Arc::clone(&stream));
        }

        tokio::spawn(Arc::clone(self).read_pump(rd, Arc::clone(&stream)));
        tracing::info!(
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
                        tracing::info!(
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
            tracing::info!(
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
        tracing::info!("tunnel close tunnel={} reason={reason}", self.id);
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
                let writer = Arc::clone(&self.writer);
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        writer.lock().await.take();
                    });
                }
            }
        }
        tracing::info!(
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

pub(super) fn go_io_error(err: &std::io::Error) -> String {
    strip_os_error(&err.to_string())
}

fn go_dial_error(target: &str, err: &std::io::Error) -> String {
    format!("dial tcp {target}: connectex: {}", go_io_error(err))
}

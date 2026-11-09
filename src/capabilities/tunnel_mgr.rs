use super::tunnel::TunnelBridge;
use super::tunnel_host::Lookup;
use super::tunnel_host::system_lookup;
use super::tunnel_host::validate_tunnel_target_host_with;
use super::tunnel_state::Frame;
use super::tunnel_state::MAX_FRAME_PAYLOAD;
use super::tunnel_state::StateInner;
use super::tunnel_state::TunnelState;
use base64::Engine;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

pub struct TunnelMgr {
    bridge: Arc<dyn TunnelBridge>,
    tunnels: Mutex<HashMap<String, Arc<TunnelState>>>,
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
            return Ok(Arc::clone(existing));
        }
        let t = Arc::new(TunnelState {
            id: tunnel_id.to_string(),
            local_port: port,
            local_host: display,
            dial_host: dial_ip,
            bridge: Arc::clone(&self.bridge),
            done: CancellationToken::new(),
            inner: Mutex::new(StateInner::default()),
        });
        tunnels.insert(tunnel_id.to_string(), Arc::clone(&t));
        drop(tunnels);
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
                        tracing::info!(
                            "tunnel data bad base64 tunnel={tunnel_id} stream={stream_id}: {err}"
                        );
                        return;
                    }
                };
                if payload.len() > MAX_FRAME_PAYLOAD {
                    tracing::info!(
                        "tunnel data oversize tunnel={tunnel_id} stream={stream_id} bytes={}",
                        payload.len()
                    );
                    let bridge = Arc::clone(&self.bridge);
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

use std::sync::Arc;
use std::time::Duration;

use handoff::capabilities::{self, consent::set_session_ledger};
use handoff::consent::{Ledger, Prompt};
use handoff::dispatch::{Ctx, Router};
use handoff::relay::{mint, Bridge};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn live_tunnel_host() {
    let relay = std::env::var("TUNNEL_LIVE_RELAY").expect("TUNNEL_LIVE_RELAY");
    let out = std::env::var("TUNNEL_LIVE_OUT").expect("TUNNEL_LIVE_OUT");
    let stop = std::env::var("TUNNEL_LIVE_STOP").expect("TUNNEL_LIVE_STOP");

    let minted = mint(&relay).await.expect("mint");
    let cancel = CancellationToken::new();
    let bridge = Bridge::dial(&relay, &minted.write_token, &cancel)
        .await
        .expect("dial");
    let mut router = Router::new();
    capabilities::register_all(&mut router, Some(bridge.clone()));
    let prompt: Prompt =
        Arc::new(|_, _| Box::pin(async { Ok(handoff::consent::Answer::ForGrant) }));
    set_session_ledger(Arc::new(Ledger::new(prompt)));
    let router = Arc::new(router);
    bridge
        .send_hello(
            "tunnel-live",
            "0.0.0-live",
            &router.kinds(),
            &router.specs(),
        )
        .await
        .expect("hello");
    bridge.start_heartbeat(cancel.clone());
    std::fs::write(&out, &minted.view_token).expect("write view token");

    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            while !std::path::Path::new(&stop).exists() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            cancel.cancel();
        });
    }

    loop {
        let cmd = match bridge.recv(&cancel).await {
            Ok(cmd) => cmd,
            Err(err) => {
                if cancel.is_cancelled() {
                    break;
                }
                eprintln!("recv: {err}; reconnecting");
                if bridge.reconnect(&cancel).await.is_err() {
                    break;
                }
                continue;
            }
        };
        if capabilities::is_tunnel_frame_kind(&cmd.kind) {
            capabilities::enqueue_tunnel_frame(&cmd.kind, &cmd.extras);
            continue;
        }
        let router = router.clone();
        let bridge = bridge.clone();
        let ctx = Ctx::new(cancel.child_token());
        tokio::spawn(async move {
            let outcome = router.dispatch(ctx, &cmd.kind, cmd.extras.clone()).await;
            eprintln!(
                "{} {} ok={} err={}",
                cmd.id, cmd.kind, outcome.ok, outcome.error
            );
            let _ = bridge.send_command_result(&cmd.id, &outcome).await;
        });
    }
    bridge.close().await;
}

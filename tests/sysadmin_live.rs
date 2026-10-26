#![cfg(windows)]

use std::process::{Command, Stdio};
use std::sync::Arc;

use handoff::capabilities::consent::set_session_ledger;
use handoff::capabilities::{app, evt, proc, ps, reg, sec, startup, storage, task};
use handoff::consent::{Ledger, Prompt};
use handoff::dispatch::{Args, Ctx, Router};
use serde_json::{json, Value};

fn args(v: Value) -> Args {
    v.as_object().cloned().unwrap_or_default()
}

fn router() -> Router {
    let mut r = Router::new();
    app::register(&mut r);
    evt::register(&mut r);
    proc::register(&mut r);
    ps::register(&mut r);
    reg::register(&mut r);
    sec::register(&mut r);
    startup::register(&mut r);
    storage::register(&mut r);
    task::register(&mut r);
    r
}

#[tokio::test]
async fn read_only_kinds_return_json_and_risky_kinds_obey_consent() {
    let prompt: Prompt =
        Arc::new(|_, _| Box::pin(async { Ok(handoff::consent::Answer::ForGrant) }));
    set_session_ledger(Arc::new(Ledger::new(prompt)));
    let r = router();

    let reads = [
        ("storage.volumes", json!({"drive_letter": "C"})),
        ("startup.list", json!({"limit": 5})),
        ("sec.local-admins", json!({})),
        ("defender.status", json!({})),
        ("update.history", json!({})),
        ("task.list", json!({"limit": 3})),
        ("app.list", json!({"limit": 3})),
        ("evt.providers", json!({"max_results": 3})),
        ("evt.snapshot", json!({"max_events": 3, "since_minutes": 5})),
        ("proc.list", json!({})),
        ("proc.find", json!({"query": "powershell", "limit": 2})),
        ("svc.list", json!({})),
        ("svc.status", json!({"name": "EventLog"})),
        (
            "reg.query",
            json!({"hive": "HKLM", "key": "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion", "value": "ProductName"}),
        ),
    ];
    for (kind, a) in reads {
        let out = r.dispatch(Ctx::default(), kind, args(a)).await;
        assert!(out.ok, "{kind}: {}", out.error);
    }

    let out = r
        .dispatch(
            Ctx::default(),
            "ps.exec",
            args(json!({"script": "Write-Output ok"})),
        )
        .await;
    assert!(out.ok, "{}", out.error);
    assert_eq!(out.result["exit_code"], json!(0));
    assert!(out.result["stdout"].as_str().unwrap().contains("ok"));

    let mut child = Command::new("ping")
        .args(["-n", "300", "127.0.0.1"])
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn ping");
    let pid = child.id();
    let out = r
        .dispatch(Ctx::default(), "proc.kill", args(json!({"pid": pid})))
        .await;
    if !out.ok {
        let _ = child.kill();
    }
    assert!(out.ok, "{}", out.error);
    assert_eq!(out.result["pid"], json!(pid));
    assert_eq!(out.result["killed"], json!(true));
    let status = child.wait().expect("wait");
    assert!(!status.success());
}

#[tokio::test]
async fn svc_control_validates_before_asking() {
    let r = router();
    for a in [
        json!({"action": "start"}),
        json!({"name": "Spooler", "action": "obliterate"}),
        json!({"name": "Spooler"}),
    ] {
        let out = r.dispatch(Ctx::default(), "svc.control", args(a)).await;
        assert!(!out.ok);
    }
}

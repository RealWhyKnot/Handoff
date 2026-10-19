pub mod app;
pub mod consent;
pub mod drv;
pub mod evt;
pub mod fs;
pub mod fs_guard;
pub mod fs_lines;
pub mod fs_text;
pub mod fs_write;
pub mod hw;
pub mod net;
pub mod net_curl;
pub mod net_diag;
pub mod net_probes;
pub mod net_resolve;
pub mod net_shares;
pub mod pico;
pub mod print;
pub mod proc;
pub mod ps;
pub mod pwsh;
pub mod pwsh_out;
pub mod reg;
pub mod screenshot;
pub mod sec;
pub mod startup;
pub mod storage;
pub mod sys;
pub mod task;
pub mod tunnel;

use std::time::Duration;

use crate::dispatch::{handler, Ctx, Handler, HandlerError, Param, ParamType, Router, Spec};
use crate::relay::Bridge;

pub use consent::{deny_all_risky, last_consent_decision, ledger};
pub use tunnel::{enqueue_tunnel_frame, is_tunnel_frame_kind};

pub fn register_all(r: &mut Router, bridge: Option<Bridge>) {
    consent::reset_risk_consent();
    sys::register(r);
    hw::register(r);
    storage::register(r);
    net::register(r);
    net_shares::register(r);
    net_diag::register(r);
    net_probes::register(r);
    net_curl::register(r);
    proc::register(r);
    evt::register(r);
    drv::register(r);
    print::register(r);
    fs::register(r);
    fs_write::register(r);
    fs_text::register(r);
    net_resolve::register(r);
    screenshot::register(r);
    reg::register(r);
    task::register(r);
    sec::register(r);
    app::register(r);
    startup::register(r);
    pico::register(r);
    ps::register(r);
    tunnel::register(r, bridge);
    register_control(r);
}

pub(crate) fn register_specs(r: &mut Router, json: &str, handlers: Vec<(&str, Handler)>) {
    let mut specs = proc::load_specs(json);
    for (kind, h) in handlers {
        let i = specs
            .iter()
            .position(|s| s.kind == kind)
            .unwrap_or_else(|| panic!("no spec for {kind}"));
        let spec = specs.swap_remove(i);
        let h = match spec.default_timeout_ms {
            0 => h,
            ms => with_time_limit(&spec.kind, ms, h),
        };
        r.register_spec(spec, h);
    }
    let orphans: Vec<&str> = specs.iter().map(|s| s.kind.as_str()).collect();
    assert!(orphans.is_empty(), "specs with no handler: {orphans:?}");
}

pub(crate) fn with_time_limit(kind: &str, ms: u32, h: Handler) -> Handler {
    let kind = kind.to_string();
    handler(move |ctx: Ctx, args| {
        let h = h.clone();
        let kind = kind.clone();
        async move {
            let child = ctx.cancel.child_token();
            let inner = Ctx {
                cancel: child.clone(),
                ..ctx
            };
            match tokio::time::timeout(Duration::from_millis(u64::from(ms)), h(inner, args)).await {
                Ok(result) => result,
                Err(_) => {
                    child.cancel();
                    Err(HandlerError::plain(format!(
                        "{kind}: no answer within {} seconds",
                        ms.div_ceil(1000)
                    )))
                }
            }
        }
    })
}

pub fn register_control(r: &mut Router) {
    r.register_spec(
        Spec {
            kind: "control.cancel".into(),
            label: "Cancel command".into(),
            description: "Ask the host to stop a running command.".into(),
            params: vec![Param::new("target_id", ParamType::String)
                .required()
                .describe("Id of the command to stop.")],
            ..Spec::default()
        },
        handler(|_ctx, _args| async {
            Err(HandlerError::plain(
                "control.cancel is handled by the session loop",
            ))
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::consent::testing::SessionGuard;
    use super::*;
    use crate::dispatch::{Args, Ctx};
    use serde_json::{json, Value};

    fn spec_router() -> Router {
        let mut r = Router::new();
        register_all(&mut r, None);
        r
    }

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    #[tokio::test]
    async fn time_limit_stops_a_slow_handler_and_cancels_it() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(
            None::<tokio_util::sync::CancellationToken>,
        ));
        let s = seen.clone();
        let slow = handler(move |ctx: Ctx, _args| {
            *s.lock().unwrap() = Some(ctx.cancel.clone());
            async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(json!("late"))
            }
        });
        let limited = with_time_limit("x.slow", 50, slow);
        let err = limited(Ctx::default(), Args::new()).await.unwrap_err();
        assert_eq!(err.message, "x.slow: no answer within 1 seconds");
        assert!(seen.lock().unwrap().as_ref().unwrap().is_cancelled());
        let fast = with_time_limit(
            "x.fast",
            5000,
            handler(|_ctx, _args| async { Ok(json!(1)) }),
        );
        assert_eq!(fast(Ctx::default(), Args::new()).await.unwrap(), json!(1));
    }

    #[test]
    fn new_read_only_kinds_carry_time_limits() {
        let r = spec_router();
        for kind in [
            "print.list",
            "print.jobs",
            "drv.problems",
            "evt.crashes",
            "net.wifi",
            "net.proxy",
            "net.profile",
            "sys.time-sync",
            "hw.disk-health",
        ] {
            let spec = r
                .spec_for(kind)
                .unwrap_or_else(|| panic!("{kind} is not registered"));
            assert!(!spec.risky, "{kind}");
            assert!(
                spec.default_timeout_ms > 0 && spec.default_timeout_ms <= 30_000,
                "{kind}"
            );
        }
    }

    #[test]
    fn registered_specs_are_internally_consistent() {
        let _guard = SessionGuard::new();
        for spec in spec_router().specs() {
            assert!(!spec.kind.is_empty(), "a spec was registered with no kind");
            let mut seen = std::collections::BTreeSet::new();
            for p in &spec.params {
                assert!(!p.name.is_empty(), "{}: a param has no name", spec.kind);
                assert!(
                    seen.insert(p.name.clone()),
                    "{}: duplicate param {}",
                    spec.kind,
                    p.name
                );
                for alias in &p.aliases {
                    assert!(
                        seen.insert(alias.clone()),
                        "{}: alias {alias} collides with another param",
                        spec.kind
                    );
                }
                if p.kind == ParamType::Enum {
                    assert!(
                        !p.enum_values.is_empty(),
                        "{}: param {} is an enum with no values",
                        spec.kind,
                        p.name
                    );
                }
                assert!(
                    !(p.required && p.default.is_some()),
                    "{}: param {} is required and also has a default",
                    spec.kind,
                    p.name
                );
                if let (Some(min), Some(max)) = (p.min, p.max) {
                    assert!(
                        min <= max,
                        "{}: param {} has min above max",
                        spec.kind,
                        p.name
                    );
                }
                if let Some(n) = p.default.as_ref().and_then(Value::as_i64) {
                    if let Some(min) = p.min {
                        assert!(
                            n >= min,
                            "{}: param {} default below min",
                            spec.kind,
                            p.name
                        );
                    }
                    if let Some(max) = p.max {
                        assert!(
                            n <= max,
                            "{}: param {} default above max",
                            spec.kind,
                            p.name
                        );
                    }
                }
                if let (Some(Value::String(s)), ParamType::Enum) = (&p.default, p.kind) {
                    assert!(
                        p.enum_values.contains(s),
                        "{}: param {} default {s} is not one of its enum values",
                        spec.kind,
                        p.name
                    );
                }
            }
        }
    }

    #[test]
    fn every_spec_is_advertised_for_every_kind() {
        let _guard = SessionGuard::new();
        let r = spec_router();
        assert_eq!(r.specs().len(), r.kinds().len());
    }

    #[test]
    fn legacy_pagination_names_still_reach_handlers() {
        let _guard = SessionGuard::new();
        let r = spec_router();
        let spec = r.spec_for("app.list").expect("app.list is registered");
        let bound = spec.bind(&args(json!({"max_results": 42}))).unwrap();
        assert_eq!(bound.args["limit"], json!(42));
        assert!(bound.ignored.is_empty());
    }

    #[test]
    fn gated_kinds_and_risky_specs_agree() {
        let _guard = SessionGuard::new();
        let r = spec_router();
        for (kind, _) in crate::consent::GATED_KINDS {
            if let Some(spec) = r.spec_for(kind) {
                assert!(
                    spec.risky,
                    "{kind} asks for consent but is not marked risky"
                );
            }
        }
        for spec in r.specs() {
            if spec.risky {
                assert!(
                    crate::consent::category_for(&spec.kind).is_some(),
                    "{} is marked risky but has no consent category",
                    spec.kind
                );
            }
        }
    }

    #[test]
    fn every_kind_has_a_label() {
        let _guard = SessionGuard::new();
        for spec in spec_router().specs() {
            assert!(!spec.label.is_empty(), "{} has no label", spec.kind);
        }
    }

    #[test]
    fn enum_params_reject_values_outside_their_set() {
        let _guard = SessionGuard::new();
        let r = spec_router();
        let spec = r
            .spec_for("svc.control")
            .expect("svc.control is registered");
        let err = spec
            .bind(&args(json!({"name": "Spooler", "action": "obliterate"})))
            .err()
            .expect("expected an enum rejection");
        for want in ["start", "stop", "restart"] {
            assert!(err.contains(want), "error should name {want}: {err}");
        }
    }

    #[tokio::test]
    async fn spec_binding_runs_before_the_handler() {
        let _guard = SessionGuard::new();
        let r = spec_router();
        let out = r
            .dispatch(
                Ctx::default(),
                "proc.kill",
                args(json!({"pid": "not-a-number"})),
            )
            .await;
        assert!(
            !out.ok,
            "a string pid must be rejected, not coerced to zero"
        );
        assert!(out.error.contains("must be a number"), "{}", out.error);
    }

    #[test]
    fn new_kinds_are_registered_and_discoverable() {
        let _guard = SessionGuard::new();
        let r = spec_router();
        for kind in [
            "fs.write",
            "fs.grep",
            "net.resolve",
            "svc.status",
            "sys.screenshot",
            "control.cancel",
        ] {
            assert!(r.spec_for(kind).is_some(), "{kind} is not registered");
        }
    }

    #[test]
    fn hello_surface_matches_the_go_client() {
        let _guard = SessionGuard::new();
        let go: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/go-hello.json")).unwrap();
        let r = spec_router();
        assert_eq!(json!(r.kinds()), go["capabilities"]);
        assert_eq!(
            serde_json::to_value(r.specs()).unwrap(),
            go["command_specs"]
        );
    }

    #[tokio::test]
    async fn control_cancel_is_advertised_but_not_dispatchable() {
        let _guard = SessionGuard::new();
        let r = spec_router();
        let out = r
            .dispatch(
                Ctx::default(),
                "control.cancel",
                args(json!({"target_id": "x"})),
            )
            .await;
        assert!(!out.ok);
        assert_eq!(out.error, "control.cancel is handled by the session loop");
    }
}

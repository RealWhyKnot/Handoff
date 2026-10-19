pub mod app;
pub mod consent;
pub mod drv;
pub mod evt;
pub mod fs;
pub mod fs_text;
pub mod fs_write;
pub mod hw;
pub mod net;
pub mod net_curl;
pub mod net_probes;
pub mod net_resolve;
pub mod net_shares;
pub mod pico;
pub mod proc;
pub mod ps;
pub mod pwsh;
pub mod reg;
pub mod screenshot;
pub mod sec;
pub mod startup;
pub mod storage;
pub mod sys;
pub mod task;
pub mod tunnel;

use crate::dispatch::{handler, HandlerError, Param, ParamType, Router, Spec};
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
    net_probes::register(r);
    net_curl::register(r);
    proc::register(r);
    evt::register(r);
    drv::register(r);
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
    fn risky_kinds_are_marked_risky_in_their_specs() {
        let _guard = SessionGuard::new();
        let r = spec_router();
        for kind in [
            "ps.exec",
            "fs.upload",
            "fs.mkdir",
            "fs.delete",
            "proc.kill",
            "svc.control",
            "tunnel.open",
            "pico.bootsel",
            "pico.flash",
            "pico.save",
            "pico.reset",
        ] {
            let spec = r
                .spec_for(kind)
                .unwrap_or_else(|| panic!("{kind} is not registered"));
            if spec.params.is_empty() && spec.label.is_empty() {
                continue;
            }
            assert!(spec.risky, "{kind} must be advertised as risky");
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

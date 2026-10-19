pub mod failure;
pub mod spec;

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use failure::{Failure, HandlerError};
pub use spec::{Bound, Clamp, Param, ParamType, Spec};

pub type Args = Map<String, Value>;
pub type CapResult = Result<Value, HandlerError>;
pub type Handler = Arc<dyn Fn(Ctx, Args) -> BoxFuture<'static, CapResult> + Send + Sync>;

#[derive(Clone, Default)]
pub struct Ctx {
    pub cancel: CancellationToken,
}

impl Ctx {
    pub fn new(cancel: CancellationToken) -> Ctx {
        Ctx { cancel }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(Ctx, Args) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = CapResult> + Send + 'static,
{
    Arc::new(move |ctx, args| Box::pin(f(ctx, args)))
}

#[derive(Default, Clone)]
pub struct Router {
    handlers: BTreeMap<String, Handler>,
    specs: BTreeMap<String, Spec>,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub ok: bool,
    pub result: Value,
    pub error: String,
    pub elapsed_ms: i64,
    pub detail: Option<Value>,
    pub clamped: Vec<Clamp>,
    pub ignored: Vec<String>,
}

struct AbortOnDrop(JoinHandle<CapResult>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Router {
    pub fn new() -> Router {
        Router::default()
    }

    pub fn register(&mut self, kind: &str, h: Handler) {
        self.handlers.insert(kind.to_string(), h);
        self.specs.insert(kind.to_string(), Spec::plain(kind));
    }

    pub fn register_spec(&mut self, spec: Spec, h: Handler) {
        self.handlers.insert(spec.kind.clone(), h);
        self.specs.insert(spec.kind.clone(), spec);
    }

    pub fn kinds(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }

    pub fn specs(&self) -> Vec<Spec> {
        self.handlers
            .keys()
            .filter_map(|k| self.specs.get(k).cloned())
            .collect()
    }

    pub fn spec_for(&self, kind: &str) -> Option<&Spec> {
        self.specs.get(kind)
    }

    pub async fn dispatch(&self, ctx: Ctx, kind: &str, args: Args) -> Outcome {
        let t0 = Instant::now();
        let elapsed = |t0: Instant| t0.elapsed().as_millis() as i64;
        let Some(h) = self.handlers.get(kind).cloned() else {
            return Outcome {
                error: format!("unknown kind: {kind}"),
                elapsed_ms: elapsed(t0),
                ..Outcome::default()
            };
        };

        let mut args = args;
        let mut clamped = Vec::new();
        let mut ignored = Vec::new();
        if let Some(spec) = self.specs.get(kind) {
            if !spec.params.is_empty() {
                match spec.bind(&args) {
                    Ok(bound) => {
                        args = bound.args;
                        clamped = bound.clamped;
                        ignored = bound.ignored;
                    }
                    Err(message) => {
                        return Outcome {
                            error: message,
                            elapsed_ms: elapsed(t0),
                            ..Outcome::default()
                        };
                    }
                }
            }
        }

        let task = AbortOnDrop(tokio::spawn(async move { h(ctx, args).await }));
        let mut task = task;
        let joined = (&mut task.0).await;
        match joined {
            Ok(Ok(result)) => Outcome {
                ok: true,
                result,
                elapsed_ms: elapsed(t0),
                clamped,
                ignored,
                ..Outcome::default()
            },
            Ok(Err(err)) => Outcome {
                ok: false,
                error: err.message,
                detail: err.detail,
                elapsed_ms: elapsed(t0),
                clamped,
                ignored,
                ..Outcome::default()
            },
            Err(join_err) => {
                let panic_text = match join_err.try_into_panic() {
                    Ok(payload) => {
                        if let Some(s) = payload.downcast_ref::<&str>() {
                            (*s).to_string()
                        } else if let Some(s) = payload.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "panic".to_string()
                        }
                    }
                    Err(_) => "task cancelled".to_string(),
                };
                Outcome {
                    error: format!("internal error handling {kind}: {panic_text}"),
                    elapsed_ms: elapsed(t0),
                    ..Outcome::default()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    fn noop() -> Handler {
        handler(|_, _| async { Ok(Value::Null) })
    }

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn list_spec() -> Spec {
        Spec {
            kind: "app.list".into(),
            params: vec![
                Param::new("limit", ParamType::Int)
                    .default_value(json!(300))
                    .range(1, 5000)
                    .aliases(&["max_results"]),
                Param::new("match", ParamType::String),
                Param::new("match_mode", ParamType::Enum)
                    .enum_of(&["contains", "prefix", "regex"])
                    .default_value(json!("contains")),
                Param::new("include_hidden", ParamType::Bool).default_value(json!(false)),
            ],
            ..Spec::default()
        }
    }

    #[test]
    fn router_kinds_returns_sorted_kinds() {
        let mut r = Router::new();
        r.register("sys.uptime", noop());
        r.register("fs.read", noop());
        r.register("net.ping", noop());
        assert_eq!(r.kinds(), vec!["fs.read", "net.ping", "sys.uptime"]);
    }

    #[tokio::test]
    async fn dispatch_returns_outcome_for_handler_error() {
        let mut r = Router::new();
        r.register(
            "boom",
            handler(|_, _| async { Err("context canceled".into()) }),
        );
        let out = r.dispatch(Ctx::default(), "boom", Args::new()).await;
        assert!(!out.ok && !out.error.is_empty());
    }

    #[tokio::test]
    async fn dispatch_unknown_kind() {
        let out = Router::new()
            .dispatch(Ctx::default(), "nope", Args::new())
            .await;
        assert!(!out.ok && !out.error.is_empty());
    }

    #[tokio::test]
    async fn dispatch_recovers_handler_panic() {
        let mut r = Router::new();
        r.register(
            "panic",
            handler(|_, _| async {
                if true {
                    panic!("nil deref in a capability");
                }
                Ok(Value::Null)
            }),
        );
        let out = r.dispatch(Ctx::default(), "panic", Args::new()).await;
        assert!(!out.ok);
        assert!(
            out.error.contains("nil deref in a capability"),
            "{}",
            out.error
        );
    }

    #[tokio::test]
    async fn dispatch_binds_before_handler() {
        let mut r = Router::new();
        let seen = Arc::new(Mutex::new(Value::Null));
        let seen_in = seen.clone();
        r.register_spec(
            list_spec(),
            handler(move |_, a| {
                let seen = seen_in.clone();
                async move {
                    *seen.lock().unwrap() = a["limit"].clone();
                    Ok(json!("ok"))
                }
            }),
        );
        let out = r
            .dispatch(Ctx::default(), "app.list", args(json!({"max_results": 7})))
            .await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(*seen.lock().unwrap(), json!(7));
    }

    #[tokio::test]
    async fn dispatch_surfaces_bind_error_without_calling_handler() {
        let mut r = Router::new();
        let called = Arc::new(AtomicBool::new(false));
        let called_in = called.clone();
        r.register_spec(
            list_spec(),
            handler(move |_, _| {
                let called = called_in.clone();
                async move {
                    called.store(true, Ordering::SeqCst);
                    Ok(Value::Null)
                }
            }),
        );
        let out = r
            .dispatch(Ctx::default(), "app.list", args(json!({"limit": "nope"})))
            .await;
        assert!(!out.ok);
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dispatch_carries_clamps_and_ignores() {
        let mut r = Router::new();
        r.register_spec(list_spec(), handler(|_, _| async { Ok(json!("ok")) }));
        let out = r
            .dispatch(
                Ctx::default(),
                "app.list",
                args(json!({"limit": 99999, "typo": 1})),
            )
            .await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.clamped.len(), 1);
        assert_eq!(out.ignored, vec!["typo".to_string()]);
    }

    #[tokio::test]
    async fn dispatch_unpacks_failure_detail() {
        let mut r = Router::new();
        r.register(
            "ps.exec",
            handler(|_, _| async {
                Err(Failure {
                    message: "powershell exited 3".into(),
                    exit_code: Some(3),
                    stderr: "boom".into(),
                    ..Failure::default()
                }
                .into())
            }),
        );
        let out = r.dispatch(Ctx::default(), "ps.exec", Args::new()).await;
        assert!(!out.ok);
        assert_eq!(out.error, "powershell exited 3");
        let detail = out.detail.expect("detail");
        assert_eq!(detail["exit_code"], json!(3));
        assert_eq!(detail["stderr"], json!("boom"));
    }

    #[tokio::test]
    async fn register_without_spec_leaves_args_untouched() {
        let mut r = Router::new();
        let got = Arc::new(Mutex::new(Value::Null));
        let got_in = got.clone();
        r.register(
            "legacy.kind",
            handler(move |_, a| {
                let got = got_in.clone();
                async move {
                    *got.lock().unwrap() = a["anything"].clone();
                    Ok(Value::Null)
                }
            }),
        );
        let out = r
            .dispatch(
                Ctx::default(),
                "legacy.kind",
                args(json!({"anything": "kept"})),
            )
            .await;
        assert!(out.ok);
        assert_eq!(*got.lock().unwrap(), json!("kept"));
    }

    #[test]
    fn specs_cover_every_registered_kind() {
        let mut r = Router::new();
        r.register("a.one", noop());
        r.register_spec(list_spec(), noop());
        let specs = r.specs();
        assert_eq!(specs.len(), r.kinds().len());
        assert_eq!(specs[0].kind, "a.one");
        assert_eq!(specs[1].kind, "app.list");
    }
}

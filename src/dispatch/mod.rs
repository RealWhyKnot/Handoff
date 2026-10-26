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

use crate::consent::{take_ask, Ask};

pub use failure::{Failure, HandlerError};
pub use spec::{Bound, Clamp, Param, ParamType, Spec};

pub type Args = Map<String, Value>;
pub type CapResult = Result<Value, HandlerError>;
pub type Handler = Arc<dyn Fn(Ctx, Args) -> BoxFuture<'static, CapResult> + Send + Sync>;
pub type Progress = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone, Default)]
pub struct Ctx {
    pub cancel: CancellationToken,
    pub progress: Option<Progress>,
    pub consent: Ask,
}

impl Ctx {
    pub fn new(cancel: CancellationToken) -> Ctx {
        Ctx {
            cancel,
            progress: None,
            consent: Ask::Default,
        }
    }

    pub fn with_progress(mut self, progress: Progress) -> Ctx {
        self.progress = Some(progress);
        self
    }

    pub fn report(&self, text: String) {
        if let Some(progress) = &self.progress {
            progress(text);
        }
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

pub struct Shown {
    pub text: String,
    pub cut: bool,
}

pub fn hidden_char(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    ) || (c.is_control() && c != '\n' && c != '\t')
}

pub fn for_display(text: &str, max_chars: usize, max_lines: usize) -> Shown {
    let mut out = String::new();
    let mut chars = 0usize;
    let mut lines = 1usize;
    let mut space_run = 0usize;
    let mut cut = false;
    for c in text.chars() {
        let c = match c {
            '\t' => ' ',
            '\u{2028}' | '\u{2029}' => '\n',
            c if hidden_char(c) => continue,
            c => c,
        };
        let c = if c == '\n' && max_lines <= 1 { ' ' } else { c };
        if c == '\n' {
            space_run = 0;
            while out.ends_with(' ') {
                out.pop();
                chars -= 1;
            }
            if out.is_empty() || out.ends_with("\n\n") {
                continue;
            }
            if lines >= max_lines {
                cut = true;
                break;
            }
            lines += 1;
        } else if c == ' ' {
            space_run += 1;
            if space_run > 8 {
                continue;
            }
        } else {
            space_run = 0;
        }
        if chars >= max_chars {
            cut = true;
            break;
        }
        out.push(c);
        chars += 1;
    }
    let kept = out.trim_end().len();
    out.truncate(kept);
    Shown { text: out, cut }
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
        let mut ctx = ctx;
        match take_ask(kind, &mut args) {
            Ok(ask) => ctx.consent = ask,
            Err(message) => {
                return Outcome {
                    error: message,
                    elapsed_ms: elapsed(t0),
                    ..Outcome::default()
                };
            }
        }
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

        let cancel = ctx.cancel.clone();
        let mut task = AbortOnDrop(tokio::spawn(async move { h(ctx, args).await }));
        let joined = tokio::select! {
            biased;
            joined = &mut task.0 => joined,
            _ = cancel.cancelled() => {
                return Outcome {
                    error: "command cancelled".to_string(),
                    elapsed_ms: elapsed(t0),
                    clamped,
                    ignored,
                    ..Outcome::default()
                };
            }
        };
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

    #[tokio::test]
    async fn a_cancelled_command_returns_without_waiting_for_its_handler() {
        let mut r = Router::new();
        r.register(
            "stuck",
            handler(|_, _| async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(Value::Null)
            }),
        );
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            c.cancel();
        });
        let started = Instant::now();
        let out = r.dispatch(Ctx::new(cancel), "stuck", Args::new()).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!out.ok);
        assert_eq!(out.error, "command cancelled");
    }

    #[tokio::test]
    async fn a_finished_command_keeps_its_result() {
        let mut r = Router::new();
        r.register("quick", handler(|_, _| async { Ok(json!("done")) }));
        let cancel = CancellationToken::new();
        let out = r
            .dispatch(Ctx::new(cancel.clone()), "quick", Args::new())
            .await;
        cancel.cancel();
        assert!(out.ok);
        assert_eq!(out.result, json!("done"));
    }

    #[test]
    fn display_text_drops_controls_and_bidi_overrides() {
        let shown = for_display(
            "a\u{202E}b\u{2066}c\u{2069}d\u{200F}e\u{1b}[31mf\u{7}g\rh\u{FEFF}i\u{0}j",
            100,
            10,
        );
        assert_eq!(shown.text, "abcde[31mfghij");
        assert!(!shown.cut);
        for c in [
            '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}',
        ] {
            assert_eq!(for_display(&format!("x{c}y"), 10, 1).text, "xy");
        }
    }

    #[test]
    fn display_text_is_cut_by_characters_and_lines() {
        let shown = for_display(&"x".repeat(50), 10, 5);
        assert_eq!(shown.text, "x".repeat(10));
        assert!(shown.cut);

        let shown = for_display("1\n2\n3\n4", 100, 2);
        assert_eq!(shown.text, "1\n2");
        assert!(shown.cut);

        let shown = for_display("C:\\a\nb", 100, 1);
        assert_eq!(shown.text, "C:\\a b");
        assert!(!shown.cut);
    }

    #[test]
    fn display_text_squeezes_padding_used_to_hide_things() {
        let shown = for_display(
            &format!("\n\n\nok{}; evil\n\n\n\n\nnext", " ".repeat(500)),
            100,
            10,
        );
        assert_eq!(shown.text, format!("ok{}; evil\n\nnext", " ".repeat(8)));
        assert!(!shown.cut);
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

    #[tokio::test]
    async fn consent_minutes_reaches_the_ctx_and_never_the_capability() {
        let mut r = Router::new();
        let got = Arc::new(Mutex::new(None));
        let got_in = got.clone();
        r.register_spec(
            list_spec(),
            handler(move |ctx: Ctx, a: Args| {
                let got = got_in.clone();
                async move {
                    *got.lock().unwrap() = Some((ctx.consent, a));
                    Ok(Value::Null)
                }
            }),
        );
        let out = r
            .dispatch(
                Ctx::default(),
                "app.list",
                args(json!({"limit": 5, "consent_minutes": 30})),
            )
            .await;
        assert!(out.ok, "{}", out.error);
        assert!(out.ignored.is_empty(), "{:?}", out.ignored);
        let (ask, seen) = got.lock().unwrap().take().unwrap();
        assert_eq!(ask, Ask::Minutes(30));
        assert!(!seen.contains_key("consent_minutes"));
        assert_eq!(seen["limit"], json!(5));

        r.register(
            "legacy.kind",
            handler(|ctx: Ctx, a: Args| async move {
                assert!(a.is_empty());
                assert_eq!(ctx.consent, Ask::Once);
                Ok(Value::Null)
            }),
        );
        let out = r
            .dispatch(
                Ctx::default(),
                "legacy.kind",
                args(json!({"consent_minutes": 0})),
            )
            .await;
        assert!(out.ok, "{}", out.error);
    }

    #[tokio::test]
    async fn bad_consent_minutes_stop_the_command_before_it_runs() {
        let mut r = Router::new();
        let called = Arc::new(AtomicBool::new(false));
        let c = called.clone();
        r.register(
            "ps.exec",
            handler(move |_, _| {
                c.store(true, Ordering::SeqCst);
                async { Ok(Value::Null) }
            }),
        );
        for bad in [json!(-5), json!(241), json!(2.5), json!("10")] {
            let out = r
                .dispatch(
                    Ctx::default(),
                    "ps.exec",
                    args(json!({"script": "Get-Date", "consent_minutes": bad})),
                )
                .await;
            assert!(!out.ok);
            assert!(out
                .error
                .contains("'consent_minutes' must be a whole number from 0 to 240"));
        }
        assert!(!called.load(Ordering::SeqCst));
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

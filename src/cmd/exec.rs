use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::flags::{parse_options, Def, FlagError};
use crate::capabilities::pwsh::normalize_numbers;
use crate::util;

pub const EXIT_USAGE: i32 = 2;
pub const EXIT_TIMEOUT: i32 = 3;
pub const EXIT_FAILED: i32 = 4;

const EXEC_FLAGS: [Def; 4] = [
    Def::list("arg", "command argument as key=value (repeatable)"),
    Def::string("args-json", "command arguments as a JSON object", ""),
    Def::duration("timeout", "how long to wait for the result", "1m0s"),
    Def::boolean("list", "list the commands this session accepts"),
];

const USAGE: &str = "usage: handoff exec <view-url-or-token> <kind> [--arg k=v]...";

type Reply = Map<String, Value>;

pub fn exec(args: &[String]) -> i32 {
    let (opts, parsed) = match parse_options("exec", args, &EXEC_FLAGS) {
        Ok(v) => v,
        Err(FlagError::Help) => return 0,
        Err(err) => {
            eprintln!("{err}");
            return EXIT_USAGE;
        }
    };
    let pairs = parsed.list("arg");
    let args_json = parsed.string("args-json");
    let timeout = parsed.duration("timeout");
    let list = parsed.boolean("list");
    let rest = parsed.args;

    if rest.is_empty() {
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    }
    let token = view_token_from(&rest[0]);

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    let deadline = Instant::now() + timeout + Duration::from_secs(15);

    if list {
        return runtime.block_on(exec_list(&opts.relay, &token, deadline));
    }
    if rest.len() < 2 {
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    }

    let payload = match build_exec_payload(&rest[1], &pairs, &args_json) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("{err}");
            return EXIT_USAGE;
        }
    };

    let result = runtime.block_on(run_command(
        &opts.relay,
        &token,
        &payload,
        timeout,
        deadline,
    ));
    let (body, status) = match result {
        Ok(v) => v,
        Err(err) => {
            eprintln!("request failed: {err}");
            return 1;
        }
    };

    let reply: Reply = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(map)) => map,
        Ok(Value::Null) => Reply::new(),
        _ => {
            eprintln!(
                "relay returned {status}: {}",
                String::from_utf8_lossy(&body).trim()
            );
            return 1;
        }
    };

    if opts.json {
        println!("{}", util::go_pretty_json(&Value::Object(reply.clone())));
    }

    match status {
        200 => {
            let ok = reply.get("ok").and_then(Value::as_bool).unwrap_or(false);
            if !opts.json {
                print_exec_result(&reply, ok);
            }
            if ok {
                0
            } else {
                EXIT_FAILED
            }
        }
        202 => {
            if !opts.json {
                eprintln!(
                    "still running after {}; fetch it later at {}",
                    util::go_duration_string(timeout),
                    reply_field(&reply, "result_url")
                );
            }
            EXIT_TIMEOUT
        }
        _ => {
            if !opts.json {
                eprintln!("relay returned {status}: {}", reply_field(&reply, "error"));
            }
            1
        }
    }
}

fn reply_field(reply: &Reply, key: &str) -> String {
    match reply.get(key) {
        Some(v) => util::go_value_string(v),
        None => "<nil>".to_string(),
    }
}

fn print_exec_result(reply: &Reply, ok: bool) {
    if !ok {
        eprintln!("command failed: {}", reply_field(reply, "error"));
        if let Some(detail) = reply.get("detail") {
            if !detail.is_null() {
                eprintln!("{}", util::go_pretty_json(detail));
            }
        }
        return;
    }
    match reply.get("result") {
        None | Some(Value::Null) => println!("ok"),
        Some(Value::String(s)) => println!("{s}"),
        Some(other) => println!("{}", util::go_pretty_json(other)),
    }
}

fn client() -> Result<reqwest::Client, String> {
    util::install_crypto_provider();
    reqwest::Client::builder()
        .build()
        .map_err(|e| e.to_string())
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn describe(err: reqwest::Error, method: &str, url: &str) -> String {
    let reason = if err.is_timeout() {
        "context deadline exceeded".to_string()
    } else {
        let mut text = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(inner) = source {
            text = format!("{text}: {inner}");
            source = inner.source();
        }
        text
    };
    format!("{method} {}: {reason}", util::go_quote(url))
}

async fn exec_list(relay: &str, token: &str, deadline: Instant) -> i32 {
    let url = format!("{relay}/api/sessions/{token}/meta");
    let http = match client() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    let resp = match http.get(&url).timeout(remaining(deadline)).send().await {
        Ok(r) => r,
        Err(err) => {
            eprintln!("request failed: {}", describe(err, "Get", &url));
            return 1;
        }
    };

    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(err) => {
            eprintln!("could not read session metadata: {err}");
            return 1;
        }
    };
    let meta: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("could not read session metadata: {err}");
            return 1;
        }
    };
    if !meta.get("ready").and_then(Value::as_bool).unwrap_or(false) {
        eprintln!("no host is connected to this session yet");
        return 1;
    }
    if let Some(kinds) = meta.get("allowed_kinds").and_then(Value::as_array) {
        for kind in kinds.iter().filter_map(Value::as_str) {
            println!("{kind}");
        }
    }
    0
}

pub fn build_exec_payload(
    kind: &str,
    pairs: &[String],
    args_json: &str,
) -> Result<Map<String, Value>, String> {
    let mut payload = Map::new();
    payload.insert("kind".to_string(), json!(kind));

    if !args_json.trim().is_empty() {
        match serde_json::from_str::<Value>(args_json) {
            Ok(Value::Object(parsed)) => {
                for (k, v) in parsed {
                    payload.insert(k, normalize_numbers(v));
                }
            }
            Ok(Value::Null) => {}
            Ok(other) => {
                let kind_name = match other {
                    Value::Array(_) => "array",
                    Value::String(_) => "string",
                    Value::Number(_) => "number",
                    _ => "bool",
                };
                return Err(format!(
                    "--args-json is not a JSON object: json: cannot unmarshal {kind_name} into Go value of type map[string]interface {{}}"
                ));
            }
            Err(err) => return Err(format!("--args-json is not a JSON object: {err}")),
        }
    }

    for pair in pairs {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) if !k.is_empty() => (k, v),
            _ => {
                return Err(format!(
                    "--arg must be key=value, got {}",
                    util::go_quote(pair)
                ))
            }
        };
        let typed = serde_json::from_str::<Value>(value)
            .map(normalize_numbers)
            .unwrap_or_else(|_| Value::String(value.to_string()));
        payload.insert(key.to_string(), typed);
    }
    Ok(payload)
}

pub fn view_token_from(input: &str) -> String {
    let s = input.trim();
    if let Some(i) = s.rfind("/v/") {
        return s[i + 3..].trim_matches('/').to_string();
    }
    s.trim_matches('/').to_string()
}

async fn post_json(
    url: &str,
    payload: &Map<String, Value>,
    deadline: Instant,
) -> Result<(Vec<u8>, u16), String> {
    let body = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let resp = client()?
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .timeout(remaining(deadline))
        .send()
        .await
        .map_err(|e| describe(e, "Post", url))?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    Ok((bytes.to_vec(), status))
}

async fn run_command(
    relay: &str,
    token: &str,
    payload: &Map<String, Value>,
    timeout: Duration,
    deadline: Instant,
) -> Result<(Vec<u8>, u16), String> {
    let run_url = format!(
        "{relay}/api/sessions/{token}/run?wait_ms={}",
        timeout.as_millis()
    );
    let (body, status) = post_json(&run_url, payload, deadline).await?;
    if status != 404 {
        return Ok((body, status));
    }

    if let Ok(Value::Object(probe)) = serde_json::from_slice::<Value>(&body) {
        let msg = probe.get("error").and_then(Value::as_str).unwrap_or("");
        if msg.contains("view token") {
            return Ok((body, status));
        }
    }
    queue_and_await(relay, token, payload, timeout, deadline).await
}

async fn queue_and_await(
    relay: &str,
    token: &str,
    payload: &Map<String, Value>,
    timeout: Duration,
    ctx_deadline: Instant,
) -> Result<(Vec<u8>, u16), String> {
    let (ack_body, ack_status) = post_json(
        &format!("{relay}/api/sessions/{token}/cmd"),
        payload,
        ctx_deadline,
    )
    .await?;
    if ack_status != 200 {
        return Ok((ack_body, ack_status));
    }

    let command_id = match serde_json::from_slice::<Value>(&ack_body) {
        Ok(v) => v
            .get("command_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Err(_) => String::new(),
    };
    if command_id.is_empty() {
        return Ok((ack_body, ack_status));
    }

    let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
    let deadline = Instant::now() + timeout;
    let result_url = format!("{relay}/api/sessions/{token}/cmd/{command_id}");
    let http = client()?;

    while Instant::now() < deadline {
        let wait = remaining(deadline).min(Duration::from_secs(25));
        let url = format!("{result_url}?wait_ms={}", wait.as_millis());
        let resp = http
            .get(&url)
            .timeout(remaining(ctx_deadline))
            .send()
            .await
            .map_err(|e| describe(e, "Get", &url))?;
        let status = resp.status().as_u16();
        let raw = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();

        let got: Reply = match serde_json::from_slice::<Value>(&raw) {
            Ok(Value::Object(map)) => map,
            _ => Reply::new(),
        };

        match status {
            200 => {
                if let Some(Value::Object(inner)) = got.get("payload") {
                    return Ok((reshape_result(inner, kind, &command_id), 200));
                }
                if got.get("status") == Some(&json!("pending")) {
                    continue;
                }
                return Ok((raw, 200));
            }
            410 => return Ok((raw, 410)),
            404 => continue,
            other => return Ok((raw, other)),
        }
    }

    let pending = json!({
        "command_id": command_id,
        "kind": kind,
        "status": "pending",
        "ok": false,
        "result_url": format!("/api/sessions/{token}/cmd/{command_id}"),
    });
    Ok((serde_json::to_vec(&pending).unwrap_or_default(), 202))
}

pub fn reshape_result(inner: &Map<String, Value>, kind: &str, command_id: &str) -> Vec<u8> {
    let ok = inner.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let field = |key: &str| inner.get(key).cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("command_id".into(), json!(command_id));
    out.insert("kind".into(), json!(kind));
    out.insert("ok".into(), json!(ok));
    out.insert("status".into(), json!(if ok { "ok" } else { "error" }));
    out.insert("result".into(), field("result"));
    out.insert("error".into(), field("error"));
    out.insert("elapsed_ms".into(), field("elapsed_ms"));
    if let Some(detail) = inner.get("detail") {
        out.insert("detail".into(), detail.clone());
    }
    serde_json::to_vec(&Value::Object(out))
        .unwrap_or_else(|_| br#"{"ok":false,"error":"could not read the relay's result"}"#.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Request {
        method: String,
        path: String,
    }

    type Responder = dyn Fn(&Request) -> (u16, String) + Send + Sync;

    fn serve(responder: Arc<Responder>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let responder = responder.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        let n = stream.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let mut first = head.lines().next().unwrap_or("").split_whitespace();
                    let method = first.next().unwrap_or("").to_string();
                    let path = first.next().unwrap_or("").to_string();
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + length {
                        let n = stream.read(&mut chunk).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let (status, body) = responder(&Request { method, path });
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                });
            }
        });
        format!("http://{addr}")
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn quiet_env() {
        std::env::set_var("HANDOFF_CONFIG", "");
    }

    #[test]
    fn build_exec_payload_types_values() {
        let payload = build_exec_payload(
            "fs.search",
            &strs(&[
                "limit=50",
                "recursive=true",
                "path=C:\\Temp",
                "pattern=*.log",
            ]),
            "",
        )
        .unwrap();
        assert_eq!(payload["kind"], json!("fs.search"));
        assert_eq!(payload["limit"].as_f64(), Some(50.0));
        assert_eq!(payload["recursive"], json!(true));
        assert_eq!(payload["path"], json!("C:\\Temp"));
        assert_eq!(payload["pattern"], json!("*.log"));
    }

    #[test]
    fn build_exec_payload_merges_args_json() {
        let payload = build_exec_payload(
            "app.list",
            &strs(&["limit=5"]),
            r#"{"name_prefix":"Micro","limit":99}"#,
        )
        .unwrap();
        assert_eq!(payload["name_prefix"], json!("Micro"));
        assert_eq!(payload["limit"].as_f64(), Some(5.0));
    }

    #[test]
    fn build_exec_payload_rejects_bad_input() {
        assert!(build_exec_payload("x", &strs(&["noequals"]), "").is_err());
        assert!(build_exec_payload("x", &strs(&["=v"]), "").is_err());
        assert!(build_exec_payload("x", &[], "[1,2]").is_err());
        assert!(build_exec_payload("x", &[], "{oops").is_err());
    }

    #[test]
    fn build_exec_payload_serializes_whole_numbers_without_a_fraction() {
        let payload = build_exec_payload("x", &strs(&["limit=3.0"]), "").unwrap();
        assert_eq!(serde_json::to_string(&payload["limit"]).unwrap(), "3");
    }

    #[test]
    fn view_token_from_accepts_url_or_token() {
        let cases = [
            ("n1_abc", "n1_abc"),
            ("https://handoff.whyknot.dev/v/n1_abc", "n1_abc"),
            ("https://handoff.whyknot.dev/v/n1_abc/", "n1_abc"),
            ("  n1_abc  ", "n1_abc"),
        ];
        for (input, want) in cases {
            assert_eq!(view_token_from(input), want, "{input:?}");
        }
    }

    #[test]
    fn exec_exit_codes() {
        quiet_env();
        let cases = [
            ("success", 200, r#"{"ok":true,"result":{"a":1}}"#, 0),
            (
                "host reported failure",
                200,
                r#"{"ok":false,"error":"boom"}"#,
                EXIT_FAILED,
            ),
            (
                "still running",
                202,
                r#"{"status":"pending","result_url":"/x"}"#,
                EXIT_TIMEOUT,
            ),
            ("cancelled", 410, r#"{"error":"command cancelled"}"#, 1),
        ];
        for (name, status, body, want) in cases {
            let body = body.to_string();
            let relay = serve(Arc::new(move |_| (status, body.clone())));
            let code = exec(&strs(&[
                "--relay", &relay, "--json", "n1_token", "sys.info",
            ]));
            assert_eq!(code, want, "{name}");
        }
    }

    #[test]
    fn exec_list_prints_allowed_kinds() {
        quiet_env();
        let relay = serve(Arc::new(|r| {
            assert!(r.path.ends_with("/meta"), "{}", r.path);
            (
                200,
                r#"{"ready":true,"allowed_kinds":["sys.info","fs.read"]}"#.into(),
            )
        }));
        assert_eq!(exec(&strs(&["--relay", &relay, "--list", "n1_token"])), 0);
    }

    #[test]
    fn exec_list_fails_when_no_host_is_connected() {
        quiet_env();
        let relay = serve(Arc::new(|_| (200, r#"{"ready":false}"#.into())));
        assert_ne!(exec(&strs(&["--relay", &relay, "--list", "n1_token"])), 0);
    }

    #[test]
    fn exec_requires_a_kind() {
        quiet_env();
        assert_eq!(
            exec(&strs(&["--relay", "https://example.test", "n1_token"])),
            EXIT_USAGE
        );
        assert_eq!(
            exec(&strs(&["--relay", "https://example.test"])),
            EXIT_USAGE
        );
    }

    #[test]
    fn exec_posts_to_run_with_wait_ms() {
        quiet_env();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        let relay = serve(Arc::new(move |r| {
            seen2
                .lock()
                .unwrap()
                .push(format!("{} {}", r.method, r.path));
            (200, r#"{"ok":true,"result":"hi"}"#.into())
        }));
        let code = exec(&strs(&[
            "--relay",
            &relay,
            "--timeout",
            "5s",
            "n1_token",
            "sys.info",
        ]));
        assert_eq!(code, 0);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["POST /api/sessions/n1_token/run?wait_ms=5000"]
        );
    }

    #[test]
    fn exec_falls_back_when_relay_has_no_run_endpoint() {
        quiet_env();
        let saw_cmd = Arc::new(AtomicUsize::new(0));
        let saw_result = Arc::new(AtomicUsize::new(0));
        let (c, r) = (saw_cmd.clone(), saw_result.clone());
        let relay = serve(Arc::new(move |req| {
            let path = req.path.split('?').next().unwrap_or("").to_string();
            if path.ends_with("/run") {
                (404, r#"{"error":"Not Found"}"#.into())
            } else if path.ends_with("/cmd") {
                c.fetch_add(1, Ordering::SeqCst);
                (200, r#"{"command_id":"c_fallback"}"#.into())
            } else if path.contains("/cmd/c_fallback") {
                r.fetch_add(1, Ordering::SeqCst);
                (
                    200,
                    r#"{"id":"c_fallback","payload":{"id":"c_fallback","ok":true,"result":{"up":1},"elapsed_ms":7}}"#
                        .into(),
                )
            } else {
                (404, String::new())
            }
        }));
        let code = exec(&strs(&[
            "--relay",
            &relay,
            "--json",
            "n1_token",
            "sys.uptime",
        ]));
        assert_eq!(code, 0);
        assert!(saw_cmd.load(Ordering::SeqCst) > 0 && saw_result.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn exec_does_not_fall_back_for_an_unknown_session() {
        quiet_env();
        let cmd_calls = Arc::new(AtomicUsize::new(0));
        let c = cmd_calls.clone();
        let relay = serve(Arc::new(move |req| {
            if req.path.ends_with("/cmd") {
                c.fetch_add(1, Ordering::SeqCst);
            }
            (404, r#"{"error":"unknown view token"}"#.into())
        }));
        let code = exec(&strs(&[
            "--relay",
            &relay,
            "--json",
            "n1_token",
            "sys.uptime",
        ]));
        assert_eq!(code, 1);
        assert_eq!(cmd_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn exec_fallback_reports_host_failure() {
        quiet_env();
        let relay = serve(Arc::new(|req| {
            let path = req.path.split('?').next().unwrap_or("");
            if path.ends_with("/run") {
                (404, r#"{"error":"Not Found"}"#.into())
            } else if path.ends_with("/cmd") {
                (200, r#"{"command_id":"c_bad"}"#.into())
            } else {
                (
                    200,
                    r#"{"id":"c_bad","payload":{"id":"c_bad","ok":false,"error":"boom"}}"#.into(),
                )
            }
        }));
        let code = exec(&strs(&["--relay", &relay, "--json", "n1_token", "ps.exec"]));
        assert_eq!(code, EXIT_FAILED);
    }

    #[test]
    fn reshape_result_matches_the_run_envelope() {
        let inner = json!({"ok": false, "error": "boom", "detail": {"a": 1}});
        let out = reshape_result(inner.as_object().unwrap(), "ps.exec", "c1");
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            out,
            json!({
                "command_id": "c1",
                "kind": "ps.exec",
                "ok": false,
                "status": "error",
                "result": null,
                "error": "boom",
                "elapsed_ms": null,
                "detail": {"a": 1}
            })
        );
    }

    #[test]
    fn unreachable_relay_is_a_request_failure() {
        quiet_env();
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let relay = format!("http://127.0.0.1:{port}");
        assert_eq!(exec(&strs(&["--relay", &relay, "n1_token", "sys.info"])), 1);
    }
}

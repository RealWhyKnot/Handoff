use super::exec_describe::describe_command;
use super::exec_describe::exec_list;
use super::exec_relay::run_command;
use super::exec_render::print_exec_result;
use super::exec_render::reply_field;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

pub use super::exec_describe::describe_spec;
pub use super::exec_relay::build_exec_payload;
pub use super::exec_relay::reshape_result;
pub use super::exec_relay::view_token_from;
pub use super::exec_render::render_table;
use super::flags::{Def, FlagError, parse_options};
use crate::console::{self, human_duration, plain_error};
use crate::util;

pub const EXIT_USAGE: i32 = 2;
pub const EXIT_TIMEOUT: i32 = 3;
pub const EXIT_FAILED: i32 = 4;

const EXEC_FLAGS: [Def; 5] = [
    Def::list("arg", "command argument as key=value (repeatable)"),
    Def::string("args-json", "command arguments as a JSON object", ""),
    Def::duration("timeout", "how long to wait for the result", "1m0s"),
    Def::boolean("list", "list the commands this session accepts"),
    Def::string(
        "describe",
        "show what a command does, its arguments, and whether it changes the computer",
        "",
    ),
];

const USAGE: &str = "usage: handoff exec <view-url-or-token> <kind> [--arg k=v]...\n       handoff exec --describe <kind>";

pub(super) type Reply = Map<String, Value>;

pub fn exec(args: &[String]) -> i32 {
    let (opts, parsed) = match parse_options("exec", args, &EXEC_FLAGS) {
        Ok(v) => v,
        Err(FlagError::Help) => return 0,
        Err(err) => {
            crate::errln!("{err}");
            return EXIT_USAGE;
        }
    };
    let pairs = parsed.list("arg");
    let args_json = parsed.string("args-json");
    let timeout = parsed.duration("timeout");
    let list = parsed.boolean("list");
    let describe_kind = parsed.string("describe");
    let rest = parsed.args;
    if !opts.json {
        console::init();
    }

    if !describe_kind.trim().is_empty() {
        return describe_command(describe_kind.trim());
    }
    if rest.is_empty() {
        crate::errln!("{USAGE}");
        return EXIT_USAGE;
    }
    let token = view_token_from(&rest[0]);

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            crate::errln!("{err}");
            return 1;
        }
    };
    let deadline = Instant::now() + timeout + Duration::from_secs(15);

    if list {
        return runtime.block_on(exec_list(&opts.relay, &token, deadline));
    }
    if rest.len() < 2 {
        crate::errln!("{USAGE}");
        return EXIT_USAGE;
    }

    let payload = match build_exec_payload(&rest[1], &pairs, &args_json) {
        Ok(p) => p,
        Err(err) => {
            crate::errln!("{err}");
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
            crate::errln!("request failed: {}", plain_error(&err));
            return 1;
        }
    };

    let reply: Reply = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(map)) => map,
        Ok(Value::Null) => Reply::new(),
        _ => {
            crate::errln!(
                "relay returned {status}: {}",
                String::from_utf8_lossy(&body).trim()
            );
            return 1;
        }
    };

    if opts.json {
        crate::outln!("{}", util::go_pretty_json(&Value::Object(reply.clone())));
    }

    match status {
        200 => {
            let ok = reply.get("ok").and_then(Value::as_bool).unwrap_or(false);
            if !opts.json {
                print_exec_result(&reply, ok);
            }
            if ok { 0 } else { EXIT_FAILED }
        }
        202 => {
            if !opts.json {
                crate::errln!(
                    "Still running after {}. Fetch the result later at {}",
                    human_duration(timeout),
                    reply_field(&reply, "result_url")
                );
            }
            EXIT_TIMEOUT
        }
        _ => {
            if !opts.json {
                crate::errln!("relay returned {status}: {}", reply_field(&reply, "error"));
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::consent::testing::SessionGuard;
    use crate::cmd::exec_describe::local_specs;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
                let responder = Arc::clone(&responder);
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
        // SAFETY: every test that touches HANDOFF_CONFIG sets it to the same empty value.
        unsafe { std::env::set_var("HANDOFF_CONFIG", "") };
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
        let seen2 = Arc::clone(&seen);
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
        let (c, r) = (Arc::clone(&saw_cmd), Arc::clone(&saw_result));
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
        let c = Arc::clone(&cmd_calls);
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
    fn array_results_print_as_a_table() {
        console::set_color(false);
        let t = render_table(&json!([
            {"Name": "explorer.exe", "Id": 4120},
            {"Name": "svchost.exe", "Id": 900, "Extra": true}
        ]))
        .unwrap();
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "Id    Name          Extra");
        assert_eq!(lines[2], "4120  explorer.exe");
        assert_eq!(lines[3], "900   svchost.exe   true");
    }

    #[test]
    fn objects_with_one_list_print_their_fields_then_a_table() {
        console::set_color(false);
        let t = render_table(&json!({
            "count": 1,
            "entries": [{"name": "a.txt", "size": 5}]
        }))
        .unwrap();
        assert_eq!(
            t,
            "count    1\n\nentries:\nname   size\n-----  ----\na.txt  5"
        );
        assert!(render_table(&json!({"a": 1})).is_none());
        assert!(render_table(&json!("text")).is_none());
        assert!(render_table(&json!([])).is_none());
    }

    #[test]
    fn describe_says_whether_a_command_changes_the_computer() {
        let _guard = SessionGuard::new();
        console::set_color(false);
        let specs = local_specs();
        let delete = specs.iter().find(|s| s.kind == "fs.delete").unwrap();
        let text = describe_spec(delete);
        assert!(text.contains("This changes the computer."), "{text}");
        assert!(text.contains("for the next 15 minutes"), "{text}");
        assert!(text.contains("path"), "{text}");
        let cpu = specs.iter().find(|s| s.kind == "hw.cpu").unwrap();
        let text = describe_spec(cpu);
        assert!(text.contains("This only reads information."), "{text}");
        assert!(text.contains("It takes no arguments."), "{text}");
    }

    #[test]
    fn describe_flag_needs_no_session() {
        let _guard = SessionGuard::new();
        quiet_env();
        assert_eq!(exec(&strs(&["--describe", "hw.cpu"])), 0);
        assert_eq!(exec(&strs(&["--describe", "hw.nothing"])), EXIT_USAGE);
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

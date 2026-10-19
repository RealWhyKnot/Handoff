use std::net::{IpAddr, SocketAddr};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tokio::net::{TcpSocket, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::net::{arg_int, arg_string};
use super::net_resolve::{ip_string, is_v4, lookup_ip_addrs};
use super::pwsh::{parse_pwsh_json, ps_single_quote, run_pwsh_echo, run_pwsh_json, Echo};

const TCP_TEST_LIMIT: usize = 30;
const TCP_TEST_WINDOW: Duration = Duration::from_secs(60);

static TCP_TESTS: LazyLock<Mutex<Vec<Instant>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/net_probes.json"),
        vec![
            ("net.ping", handler(net_ping)),
            ("net.trace", handler(net_trace)),
            ("net.tcp-test", handler(net_tcp_test)),
        ],
    );
}

pub fn valid_target(s: &str) -> bool {
    s.parse::<IpAddr>().is_ok() || valid_hostname(s)
}

fn valid_hostname(s: &str) -> bool {
    let name = s.strip_suffix('.').unwrap_or(s);
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    let labels_ok = name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    let last = name.rsplit('.').next().unwrap_or(name);
    labels_ok && !last.bytes().all(|b| b.is_ascii_digit())
}

fn take_tcp_test_slot(now: Instant) -> Result<(), HandlerError> {
    let mut history = TCP_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    history.retain(|t| now.saturating_duration_since(*t) < TCP_TEST_WINDOW);
    if history.len() >= TCP_TEST_LIMIT {
        return Err(HandlerError::plain(format!(
            "net.tcp-test: rate limit reached ({TCP_TEST_LIMIT} per minute)"
        )));
    }
    history.push(now);
    Ok(())
}

pub fn ping_script(args: &Args) -> Result<String, HandlerError> {
    let target = arg_string(args, "target").unwrap_or_default();
    let mut count = arg_int(args, "count").unwrap_or(4);
    if count <= 0 || count > 10 {
        count = 4;
    }
    if !valid_target(&target) {
        return Err(HandlerError::plain("net.ping: invalid 'target'"));
    }
    let target = ps_single_quote(&target);
    Ok(format!(
        "Test-Connection -ComputerName {target} -Count {count} -ErrorAction SilentlyContinue |\n\t\tForEach-Object {{ [Console]::Error.WriteLine(('Reply from {{0}}: {{1}} ms' -f $_.Address, $_.ResponseTime)); $_ }} |\n\t\tSelect-Object Address,@{{n='ResponseTimeMs';e={{$_.ResponseTime}}}},StatusCode |\n\t\tConvertTo-Json -Compress"
    ))
}

pub fn trace_script(args: &Args) -> Result<String, HandlerError> {
    let target = arg_string(args, "target").unwrap_or_default();
    if !valid_target(&target) {
        return Err(HandlerError::plain("net.trace: invalid 'target'"));
    }
    let target = ps_single_quote(&target);
    Ok(format!(
        "Test-NetConnection -ComputerName {target} -TraceRoute -InformationLevel Detailed -WarningAction SilentlyContinue |\n\t\tSelect-Object ComputerName,RemoteAddress,PingSucceeded,PingReplyDetails,TraceRoute |\n\t\tConvertTo-Json -Compress -Depth 4"
    ))
}

async fn net_ping(ctx: Ctx, args: Args) -> CapResult {
    let script = ping_script(&args)?;
    let res = run_pwsh_echo(&ctx.cancel, &script, ctx.progress.clone(), Echo::Stderr)
        .await
        .map_err(|(_, failure)| HandlerError::from(failure))?;
    parse_pwsh_json(&res.stdout).map_err(HandlerError::from)
}

async fn net_trace(ctx: Ctx, args: Args) -> CapResult {
    let script = trace_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

#[derive(Debug, Clone, PartialEq)]
pub struct TcpTestOptions {
    pub target: String,
    pub port: i64,
    pub timeout_ms: i64,
}

pub fn parse_tcp_test_options(args: &Args) -> Result<TcpTestOptions, HandlerError> {
    let mut opts = TcpTestOptions {
        target: arg_string(args, "target").unwrap_or_default(),
        port: arg_int(args, "port").unwrap_or(0),
        timeout_ms: arg_int(args, "timeout_ms").unwrap_or(5000),
    };
    opts.target = opts.target.trim().to_string();
    if !valid_target(&opts.target) {
        return Err(HandlerError::plain("net.tcp-test: invalid 'target'"));
    }
    if opts.port <= 0 || opts.port > 65535 {
        return Err(HandlerError::plain("net.tcp-test: port must be 1-65535"));
    }
    opts.timeout_ms = opts.timeout_ms.clamp(1000, 30000);
    Ok(opts)
}

fn join_host_port(host: &str, port: i64) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn dial_error_text(err: &std::io::Error) -> String {
    let text = err.to_string();
    let text = match text.rfind(" (os error ") {
        Some(i) => text[..i].to_string(),
        None => text,
    };
    format!("connectex: {text}")
}

#[cfg(windows)]
fn fail_fast_on_loopback(socket: &TcpSocket, target: &SocketAddr) {
    use std::os::windows::io::AsRawSocket;

    #[repr(C)]
    struct InitialRtoParameters {
        rtt: u16,
        max_syn_retransmissions: u8,
    }

    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAIoctl(
            s: usize,
            code: u32,
            input: *const std::ffi::c_void,
            input_len: u32,
            output: *mut std::ffi::c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut std::ffi::c_void,
            completion: *mut std::ffi::c_void,
        ) -> i32;
    }

    let loopback = match target.ip() {
        IpAddr::V4(a) => a.octets()[0] == 127,
        IpAddr::V6(a) => a == std::net::Ipv6Addr::LOCALHOST,
    };
    if !loopback {
        return;
    }
    const SIO_TCP_INITIAL_RTO: u32 = 0x8000_0000 | 0x1800_0000 | 17;
    let params = InitialRtoParameters {
        rtt: u16::MAX,
        max_syn_retransmissions: 0xfe,
    };
    let mut returned = 0u32;
    unsafe {
        WSAIoctl(
            socket.as_raw_socket() as usize,
            SIO_TCP_INITIAL_RTO,
            &params as *const InitialRtoParameters as *const std::ffi::c_void,
            std::mem::size_of::<InitialRtoParameters>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

async fn connect_one(target: SocketAddr) -> std::io::Result<TcpStream> {
    let socket = if target.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    #[cfg(windows)]
    fail_fast_on_loopback(&socket, &target);
    socket.connect(target).await
}

async fn dial_serial(
    cancel: &CancellationToken,
    addrs: &[IpAddr],
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, String> {
    let deadline = Instant::now() + timeout;
    let first_is_v4 = addrs.first().map(|a| is_v4(*a)).unwrap_or(true);
    let mut ordered: Vec<IpAddr> = addrs
        .iter()
        .copied()
        .filter(|a| is_v4(*a) == first_is_v4)
        .collect();
    ordered.extend(addrs.iter().copied().filter(|a| is_v4(*a) != first_is_v4));

    let mut first_err: Option<String> = None;
    let total = ordered.len();
    for (i, ip) in ordered.into_iter().enumerate() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let shown = join_host_port(&ip_string(ip), i64::from(port));
        if remaining.is_zero() {
            let e = format!("dial tcp {shown}: i/o timeout");
            return Err(first_err.unwrap_or(e));
        }
        let left = (total - i) as u32;
        let mut slice = remaining / left;
        let sane = Duration::from_secs(2);
        if slice < sane {
            slice = if remaining < sane { remaining } else { sane };
        }
        let target = SocketAddr::new(ip, port);
        let attempt: Result<(), String> = tokio::select! {
            _ = cancel.cancelled() => Err(format!("dial tcp {shown}: operation was canceled")),
            r = tokio::time::timeout(slice, connect_one(target)) => match r {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(e)) => Err(format!("dial tcp {shown}: {}", dial_error_text(&e))),
                Err(_) => Err(format!("dial tcp {shown}: i/o timeout")),
            },
        };
        if let Err(e) = attempt {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    Err(first_err.unwrap_or_else(|| "dial tcp: no addresses".to_string()))
}

pub async fn tcp_test(cancel: &CancellationToken, opts: &TcpTestOptions) -> Value {
    let start = Instant::now();
    let timeout = Duration::from_millis(opts.timeout_ms as u64);
    let mut result = Map::new();
    result.insert("target".into(), json!(opts.target));
    result.insert("port".into(), json!(opts.port));
    result.insert("timeout_ms".into(), json!(opts.timeout_ms));
    result.insert("tcp_test_succeeded".into(), json!(false));

    let deadline = tokio::time::Instant::now() + timeout;
    let looked_up = lookup_ip_addrs(cancel, &opts.target, Some(deadline)).await;
    let (addrs, lookup_err) = match looked_up {
        Ok(a) => (a, None),
        Err(e) => (Vec::new(), Some(e)),
    };
    let resolved: Vec<String> = addrs.iter().map(|a| ip_string(*a)).collect();
    result.insert("resolved_addresses".into(), json!(resolved));
    if let Some(e) = lookup_err {
        result.insert("error".into(), json!(e));
        result.insert(
            "elapsed_ms".into(),
            json!(start.elapsed().as_millis() as i64),
        );
        return Value::Object(result);
    }
    if let Some(first) = resolved.first() {
        result.insert(
            "remote_address".into(),
            json!(join_host_port(first, opts.port)),
        );
    }

    match dial_serial(cancel, &addrs, opts.port as u16, timeout).await {
        Err(e) => {
            result.insert("error".into(), json!(e));
            result.insert(
                "elapsed_ms".into(),
                json!(start.elapsed().as_millis() as i64),
            );
        }
        Ok(stream) => {
            result.insert("tcp_test_succeeded".into(), json!(true));
            if let Ok(peer) = stream.peer_addr() {
                result.insert("remote_address".into(), json!(peer.to_string()));
            }
            if let Ok(local) = stream.local_addr() {
                result.insert("local_address".into(), json!(local.to_string()));
            }
            result.insert(
                "elapsed_ms".into(),
                json!(start.elapsed().as_millis() as i64),
            );
        }
    }
    Value::Object(result)
}

async fn net_tcp_test(ctx: Ctx, args: Args) -> CapResult {
    let opts = parse_tcp_test_options(&args)?;
    take_tcp_test_slot(Instant::now())?;
    Ok(tcp_test(&ctx.cancel, &opts).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    static LIMIT_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn msg<T>(r: Result<T, HandlerError>) -> String {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.message,
        }
    }

    #[test]
    fn registers_the_probe_kinds() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["net.ping", "net.tcp-test", "net.trace"]);
        for spec in r.specs() {
            assert!(!spec.label.is_empty(), "{} has no label", spec.kind);
        }
    }

    #[test]
    fn valid_target_accepts_hosts_and_ips() {
        for ok in [
            "example.com",
            "example.com.",
            "1.1.1.1",
            "::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "host-name.local",
            "a_b",
            "_sip._tcp.example.com",
            "localhost",
            "xn--bcher-kva.example",
        ] {
            assert!(valid_target(ok), "{ok}");
        }
        for bad in [
            "",
            "a b",
            "a;b",
            "a'b",
            "a\"b",
            "a&b",
            "a|b",
            "a`b",
            "a$b",
            "a<b",
            "a>b",
            "a\nb",
            "a\tb",
            "a..b",
            ".a",
            "-a.com",
            "a-.com",
            "a/b",
            "a\\b",
            "a:b",
            "a%b",
            "a(b)",
            "a{b}",
            "a@b",
            "a,b",
            "1.2.3",
            "127.1",
            "999.1.1.1",
            "[::1]",
            "fe80::1%12",
        ] {
            assert!(!valid_target(bad), "{bad:?}");
        }
        for q in ['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
            let hostile = format!("x{q};calc;{q}");
            assert!(!valid_target(&hostile), "{hostile:?}");
            assert!(!valid_target(&format!("ex{q}ample.com")), "{q:?}");
        }
        assert!(!valid_target("b\u{fc}cher.example"));
        let label = "a".repeat(63);
        assert!(valid_target(&label));
        assert!(!valid_target(&"a".repeat(64)));
        let long = format!("{label}.{label}.{label}.{}", "a".repeat(61));
        assert_eq!(long.len(), 253);
        assert!(valid_target(&long));
        assert!(!valid_target(&format!("{long}a")));
    }

    #[test]
    fn scripts_quote_the_target() {
        let s = ping_script(&args(json!({"target": "2001:db8::1"}))).unwrap();
        assert!(s.starts_with("Test-Connection -ComputerName '2001:db8::1' -Count 4 "));
        let s = trace_script(&args(json!({"target": "_sip._tcp.example.com"}))).unwrap();
        assert!(
            s.starts_with("Test-NetConnection -ComputerName '_sip._tcp.example.com' -TraceRoute")
        );
        for q in ['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
            let target = format!("x{q};calc;{q}");
            assert!(ping_script(&args(json!({ "target": target }))).is_err());
            assert!(trace_script(&args(json!({ "target": target }))).is_err());
        }
    }

    #[test]
    fn tcp_tests_are_rate_limited() {
        let _serial = LIMIT_TESTS.blocking_lock();
        TCP_TESTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let start = Instant::now();
        for _ in 0..TCP_TEST_LIMIT {
            take_tcp_test_slot(start).unwrap();
        }
        let err = take_tcp_test_slot(start).unwrap_err();
        assert_eq!(
            err.message,
            "net.tcp-test: rate limit reached (30 per minute)"
        );
        take_tcp_test_slot(start + TCP_TEST_WINDOW).unwrap();
        TCP_TESTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    #[tokio::test]
    async fn the_handler_refuses_once_the_limit_is_hit() {
        let _serial = LIMIT_TESTS.lock().await;
        let mut r = Router::new();
        register(&mut r);
        {
            let mut h = TCP_TESTS.lock().unwrap_or_else(|e| e.into_inner());
            h.clear();
            h.extend(std::iter::repeat(Instant::now()).take(TCP_TEST_LIMIT));
        }
        let out = r
            .dispatch(
                Ctx::default(),
                "net.tcp-test",
                args(json!({"target": "127.0.0.1", "port": 9})),
            )
            .await;
        TCP_TESTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
        assert!(!out.ok);
        assert!(out.error.contains("rate limit"), "{}", out.error);
    }

    #[test]
    fn ping_script_clamps_count_and_validates_target() {
        let s = ping_script(&args(json!({"target": "1.1.1.1", "count": 3}))).unwrap();
        assert!(s.starts_with(
            "Test-Connection -ComputerName '1.1.1.1' -Count 3 -ErrorAction SilentlyContinue |"
        ));
        assert!(s.contains(
            "Select-Object Address,@{n='ResponseTimeMs';e={$_.ResponseTime}},StatusCode |"
        ));
        assert!(s.contains("[Console]::Error.WriteLine(('Reply from {0}: {1} ms'"));
        let s = ping_script(&args(json!({"target": "1.1.1.1", "count": 99}))).unwrap();
        assert!(s.contains("-Count 4 "));
        let s = ping_script(&args(json!({"target": "1.1.1.1", "count": 0}))).unwrap();
        assert!(s.contains("-Count 4 "));
        assert_eq!(
            msg(ping_script(&args(json!({"target": "a;b"})))),
            "net.ping: invalid 'target'"
        );
        assert_eq!(msg(ping_script(&Args::new())), "net.ping: invalid 'target'");
    }

    #[test]
    fn trace_script_validates_target() {
        let s = trace_script(&args(json!({"target": "example.com"}))).unwrap();
        assert!(s.contains("-ComputerName 'example.com' -TraceRoute"));
        assert!(s.ends_with("ConvertTo-Json -Compress -Depth 4"));
        assert_eq!(
            msg(trace_script(&args(json!({"target": "x y"})))),
            "net.trace: invalid 'target'"
        );
    }

    #[test]
    fn parse_tcp_test_options_defaults_and_clamps() {
        let opts = parse_tcp_test_options(&args(json!({
            "target": " example.com ",
            "port": 443,
            "timeout_ms": 20,
        })))
        .unwrap();
        assert_eq!(opts.target, "example.com");
        assert_eq!(opts.port, 443);
        assert_eq!(opts.timeout_ms, 1000);

        let opts = parse_tcp_test_options(&args(json!({
            "target": "example.com",
            "port": 443,
            "timeout_ms": 60000,
        })))
        .unwrap();
        assert_eq!(opts.timeout_ms, 30000);

        let opts =
            parse_tcp_test_options(&args(json!({"target": "example.com", "port": 80}))).unwrap();
        assert_eq!(opts.timeout_ms, 5000);
    }

    #[test]
    fn parse_tcp_test_options_rejects_bad_input() {
        let m = msg(parse_tcp_test_options(&args(
            json!({"target": "example.com; rm", "port": 443}),
        )));
        assert!(m.contains("target"), "{m}");
        let m = msg(parse_tcp_test_options(&args(
            json!({"target": "example.com", "port": 70000}),
        )));
        assert!(m.contains("port"), "{m}");
        let m = msg(parse_tcp_test_options(&args(
            json!({"target": "example.com"}),
        )));
        assert!(m.contains("port"), "{m}");
    }

    #[tokio::test]
    async fn tcp_test_reaches_a_local_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let out = tcp_test(
            &CancellationToken::new(),
            &TcpTestOptions {
                target: "127.0.0.1".into(),
                port: i64::from(port),
                timeout_ms: 2000,
            },
        )
        .await;
        assert_eq!(out["tcp_test_succeeded"], json!(true), "{out}");
        assert_eq!(out["resolved_addresses"], json!(["127.0.0.1"]));
        assert_eq!(out["remote_address"], json!(format!("127.0.0.1:{port}")));
        assert!(out["local_address"]
            .as_str()
            .unwrap()
            .starts_with("127.0.0.1:"));
        assert!(out.get("error").is_none());
        assert_eq!(out["timeout_ms"], json!(2000));
    }

    #[tokio::test]
    async fn tcp_test_reports_a_refused_port_without_erroring() {
        let port = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let out = tcp_test(
            &CancellationToken::new(),
            &TcpTestOptions {
                target: "127.0.0.1".into(),
                port: i64::from(port),
                timeout_ms: 2000,
            },
        )
        .await;
        assert_eq!(out["tcp_test_succeeded"], json!(false), "{out}");
        let err = out["error"].as_str().unwrap();
        assert!(
            err.starts_with(&format!("dial tcp 127.0.0.1:{port}: ")),
            "{err}"
        );
        assert_eq!(out["remote_address"], json!(format!("127.0.0.1:{port}")));
        assert!(out.get("local_address").is_none());
    }

    #[tokio::test]
    async fn tcp_test_reports_lookup_failure() {
        let out = tcp_test(
            &CancellationToken::new(),
            &TcpTestOptions {
                target: "this-name-should-not-exist.invalid".into(),
                port: 80,
                timeout_ms: 5000,
            },
        )
        .await;
        assert_eq!(out["tcp_test_succeeded"], json!(false));
        assert_eq!(out["resolved_addresses"], json!([]));
        assert!(out["error"]
            .as_str()
            .unwrap()
            .starts_with("lookup this-name-should-not-exist.invalid: "));
        assert!(out.get("remote_address").is_none());
    }
}

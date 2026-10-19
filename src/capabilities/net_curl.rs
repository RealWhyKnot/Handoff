use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use base64::Engine;
use reqwest::header::{HeaderMap, LOCATION, REFERER, USER_AGENT};
use reqwest::Method;
use serde_json::{json, Map, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};
use crate::util::go_quote;

use super::net::arg_string;
use super::net_resolve::{go_ip, lookup_ip_addrs};

const BODY_CAP: usize = 1 << 20;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_VIA: usize = 5;
const USER_AGENT_VALUE: &str = "Handoff/0.1 (+https://github.com/RealWhyKnot/Handoff)";

pub fn register(r: &mut Router) {
    r.register("net.curl", handler(net_curl));
}

pub fn is_public_unicast(ip: IpAddr) -> bool {
    match go_ip(ip) {
        IpAddr::V4(a) => {
            let o = a.octets();
            if o[0] == 127
                || (o[0] == 169 && o[1] == 254)
                || (o[0] == 224 && o[1] == 0 && o[2] == 0)
                || (o[0] & 0xf0) == 224
                || a.is_unspecified()
                || o[0] == 10
                || (o[0] == 172 && o[1] & 0xf0 == 16)
                || (o[0] == 192 && o[1] == 168)
            {
                return false;
            }
            if o[0] == 100 && o[1] & 0xc0 == 64 {
                return false;
            }
            true
        }
        IpAddr::V6(a) => {
            let o = a.octets();
            if a.is_loopback()
                || a.is_unspecified()
                || (a.segments()[0] & 0xffc0) == 0xfe80
                || o[0] == 0xff
                || (o[0] & 0xfe) == 0xfc
            {
                return false;
            }
            true
        }
    }
}

fn go_status_text(code: u16) -> &'static str {
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        414 => "Request URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Requested Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "",
    }
}

fn canonical_header_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = true;
    for c in name.chars() {
        if upper {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(c.to_ascii_lowercase());
        }
        upper = c == '-';
    }
    out
}

fn header_map(headers: &HeaderMap) -> Map<String, Value> {
    let mut out: Map<String, Value> = Map::new();
    let mut seen: Vec<String> = Vec::new();
    for name in headers.keys() {
        if name.as_str() == "transfer-encoding" {
            continue;
        }
        let key = canonical_header_key(name.as_str());
        if seen.contains(&key) {
            continue;
        }
        let joined = headers
            .get_all(name)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        seen.push(key.clone());
        out.insert(key, Value::String(joined));
    }
    out
}

fn parse_go_url(raw: &str) -> Result<Url, String> {
    let bad = |msg: &str| format!("net.curl: bad url: parse {}: {msg}", go_quote(raw));
    if raw.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
        return Err(bad("net/url: invalid control character in URL"));
    }
    let mut scheme = String::new();
    for (i, c) in raw.char_indices() {
        if c.is_ascii_alphabetic() {
            continue;
        }
        if c.is_ascii_digit() || c == '+' || c == '-' || c == '.' {
            if i == 0 {
                break;
            }
            continue;
        }
        if c == ':' {
            if i == 0 {
                return Err(bad("missing protocol scheme"));
            }
            scheme = raw[..i].to_lowercase();
        }
        break;
    }
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "net.curl: scheme must be http or https (got {})",
            go_quote(&scheme)
        ));
    }
    match Url::parse(raw) {
        Ok(u) => Ok(u),
        Err(url::ParseError::EmptyHost) => Err(format!(
            "net.curl: resolve {}: lookup : no such host",
            go_quote("")
        )),
        Err(e) => Err(bad(&e.to_string())),
    }
}

fn hostname(u: &Url) -> String {
    match u.host() {
        Some(Host::Domain(d)) => d.to_string(),
        Some(Host::Ipv4(a)) => a.to_string(),
        Some(Host::Ipv6(a)) => a.to_string(),
        None => String::new(),
    }
}

fn shown_url(u: &Url, first_raw: Option<&str>) -> String {
    if u.password().is_some() {
        let mut masked = u.clone();
        let _ = masked.set_password(Some("***"));
        return masked.to_string();
    }
    match first_raw {
        Some(raw) => raw.to_string(),
        None => u.to_string(),
    }
}

fn error_chain(err: &reqwest::Error) -> String {
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(inner) = source {
        let s = inner.to_string();
        let s = match s.rfind(" (os error ") {
            Some(i) => s[..i].to_string(),
            None => s,
        };
        text = format!("{text}: {s}");
        source = inner.source();
    }
    text
}

fn op_name(method: &Method) -> &'static str {
    if method == Method::HEAD {
        "Head"
    } else {
        "Get"
    }
}

async fn guard_lookup(
    cancel: &CancellationToken,
    host: &str,
    deadline: Option<Instant>,
    allow_private: bool,
) -> Result<Vec<IpAddr>, GuardError> {
    let addrs = lookup_ip_addrs(cancel, host, deadline)
        .await
        .map_err(GuardError::Lookup)?;
    if !allow_private {
        for a in &addrs {
            if !is_public_unicast(*a) {
                return Err(GuardError::Private(go_ip(*a).to_string()));
            }
        }
    }
    Ok(addrs)
}

enum GuardError {
    Lookup(String),
    Private(String),
}

fn build_client(host: &str, addrs: &[IpAddr], port: u16) -> Result<reqwest::Client, String> {
    crate::util::install_crypto_provider();
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if host.parse::<IpAddr>().is_err() {
        let socks: Vec<SocketAddr> = addrs.iter().map(|ip| SocketAddr::new(*ip, port)).collect();
        builder = builder.resolve_to_addrs(host, &socks);
    }
    builder.build().map_err(|e| e.to_string())
}

fn is_redirect(code: u16) -> bool {
    matches!(code, 301 | 302 | 303 | 307 | 308)
}

pub async fn run_curl(
    cancel: &CancellationToken,
    url_str: &str,
    method: &str,
    allow_private: bool,
    timeout: Duration,
) -> CapResult {
    let http_method = if method == "HEAD" {
        Method::HEAD
    } else {
        Method::GET
    };
    let op = op_name(&http_method);

    let mut cur = parse_go_url(url_str).map_err(HandlerError::plain)?;
    let host = hostname(&cur);
    let mut addrs = match guard_lookup(cancel, &host, None, allow_private).await {
        Ok(a) => a,
        Err(GuardError::Lookup(e)) => {
            return Err(HandlerError::plain(format!(
                "net.curl: resolve {}: {e}",
                go_quote(&host)
            )))
        }
        Err(GuardError::Private(ip)) => {
            return Err(HandlerError::plain(format!(
                "net.curl: refusing private/loopback destination {ip}"
            )))
        }
    };

    let deadline = Instant::now() + timeout;
    let mut hops = 0usize;
    let mut first = true;
    let mut referer: Option<String> = None;

    let mut resp = loop {
        let shown = shown_url(&cur, if first { Some(url_str) } else { None });
        let fail = |msg: String| HandlerError::plain(format!("{op} {}: {msg}", go_quote(&shown)));
        let cur_host = hostname(&cur);
        let port = cur.port_or_known_default().unwrap_or(80);
        let client = build_client(&cur_host, &addrs, port).map_err(fail)?;
        let mut req = client
            .request(http_method.clone(), cur.clone())
            .header(USER_AGENT, USER_AGENT_VALUE);
        if let Some(r) = &referer {
            req = req.header(REFERER, r.as_str());
        }
        hops += 1;
        let sent = tokio::select! {
            _ = cancel.cancelled() => return Err(fail("context canceled".to_string())),
            r = tokio::time::timeout_at(deadline, req.send()) => r,
        };
        let resp =
            match sent {
                Err(_) => return Err(fail(
                    "context deadline exceeded (Client.Timeout exceeded while awaiting headers)"
                        .to_string(),
                )),
                Ok(Err(e)) => return Err(fail(error_chain(&e))),
                Ok(Ok(r)) => r,
            };

        let code = resp.status().as_u16();
        let loc = resp
            .headers()
            .get(LOCATION)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .unwrap_or_default();
        if !is_redirect(code) || loc.is_empty() {
            break resp;
        }

        let next = match cur.join(&loc) {
            Ok(u) => u,
            Err(e) => {
                return Err(fail(format!(
                    "failed to parse Location header {}: {e}",
                    go_quote(&loc)
                )))
            }
        };
        if hops >= MAX_VIA {
            return Err(fail("too many redirects".to_string()));
        }
        let next_host = hostname(&next);
        match guard_lookup(cancel, &next_host, Some(deadline), allow_private).await {
            Ok(a) => addrs = a,
            Err(GuardError::Lookup(e)) => return Err(fail(e)),
            Err(GuardError::Private(ip)) => {
                return Err(fail(format!("redirect to private dest {ip}")))
            }
        }
        if next.scheme() != "http" && next.scheme() != "https" {
            let next_shown = shown_url(&next, None);
            return Err(HandlerError::plain(format!(
                "{op} {}: unsupported protocol scheme {}",
                go_quote(&next_shown),
                go_quote(next.scheme())
            )));
        }
        referer = if cur.scheme() == "https" && next.scheme() == "http" {
            None
        } else {
            let mut r = cur.clone();
            let _ = r.set_username("");
            let _ = r.set_password(None);
            Some(r.to_string())
        };
        cur = next;
        first = false;
    };

    let status = resp.status().as_u16();
    let headers = header_map(resp.headers());

    let mut body: Vec<u8> = Vec::new();
    if http_method == Method::GET {
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => break,
                r = tokio::time::timeout_at(deadline, resp.chunk()) => r,
            };
            match chunk {
                Ok(Ok(Some(c))) => {
                    body.extend_from_slice(&c);
                    if body.len() > BODY_CAP {
                        break;
                    }
                }
                _ => break,
            }
        }
    }
    let mut truncated = false;
    if body.len() > BODY_CAP {
        body.truncate(BODY_CAP);
        truncated = true;
    }

    let mut out = Map::new();
    out.insert("status".into(), json!(status));
    out.insert(
        "status_text".into(),
        json!(format!("{status} {}", go_status_text(status))),
    );
    out.insert("headers".into(), Value::Object(headers));
    out.insert("body_size".into(), json!(body.len()));
    out.insert("body_truncated".into(), json!(truncated));
    if http_method == Method::GET {
        out.insert(
            "body_base64".into(),
            json!(base64::engine::general_purpose::STANDARD.encode(&body)),
        );
    }
    Ok(Value::Object(out))
}

async fn net_curl(ctx: Ctx, args: Args) -> CapResult {
    let url_str = arg_string(&args, "url").unwrap_or_default();
    let mut method = "GET".to_string();
    if args.contains_key("method") {
        if let Some(m) = arg_string(&args, "method") {
            method = m;
        }
        method = method.to_uppercase();
    }
    if method != "GET" && method != "HEAD" {
        return Err(HandlerError::plain(format!(
            "net.curl: method must be GET or HEAD (got {})",
            go_quote(&method)
        )));
    }
    if url_str.is_empty() {
        return Err(HandlerError::plain("net.curl: 'url' is required"));
    }
    run_curl(&ctx.cancel, &url_str, &method, false, CLIENT_TIMEOUT).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    async fn serve() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let mut lines = text.split("\r\n");
                    let first = lines.next().unwrap_or("");
                    let mut parts = first.split(' ');
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    let referer = lines
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("referer: ")
                                .map(|_| l[9..].to_string())
                        })
                        .unwrap_or_default();
                    let ua = text
                        .split("\r\n")
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("user-agent: ")
                                .map(|_| l[12..].to_string())
                        })
                        .unwrap_or_default();
                    let resp: Vec<u8> = match path.as_str() {
                        "/small" => b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Multi: 1\r\nX-Multi: 2\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec(),
                        "/big" => {
                            let mut v = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                BODY_CAP + 5000
                            )
                            .into_bytes();
                            v.extend(std::iter::repeat(b'a').take(BODY_CAP + 5000));
                            v
                        }
                        "/exact" => {
                            let mut v = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {BODY_CAP}\r\nConnection: close\r\n\r\n"
                            )
                            .into_bytes();
                            v.extend(std::iter::repeat(b'b').take(BODY_CAP));
                            v
                        }
                        "/loop" => b"HTTP/1.1 302 Found\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        "/to-small" => b"HTTP/1.1 301 Moved Permanently\r\nLocation: /small\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        "/no-location" => b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        "/echo" => {
                            let body = format!("{method}|{referer}|{ua}");
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .into_bytes()
                        }
                        "/to-echo" => b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /echo\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        "/to-ftp" => b"HTTP/1.1 302 Found\r\nLocation: ftp://127.0.0.1/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        "/stall-headers" => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            return;
                        }
                        "/stall-body" => {
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial")
                                .await;
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            return;
                        }
                        _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (port, hits)
    }

    async fn fetch(port: u16, path: &str, method: &str, timeout: Duration) -> CapResult {
        run_curl(
            &CancellationToken::new(),
            &format!("http://127.0.0.1:{port}{path}"),
            method,
            true,
            timeout,
        )
        .await
    }

    fn body_of(v: &Value) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(v["body_base64"].as_str().unwrap())
            .unwrap()
    }

    #[test]
    fn registers_net_curl() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["net.curl"]);
    }

    #[test]
    fn public_unicast_matches_go_rules() {
        for bad in [
            "127.0.0.1",
            "127.9.9.9",
            "::1",
            "0.0.0.0",
            "::",
            "169.254.1.1",
            "fe80::1",
            "224.0.0.1",
            "224.5.5.5",
            "239.1.1.1",
            "ff02::1",
            "ff05::1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "fc00::1",
            "fd12:3456::1",
            "100.64.0.1",
            "100.127.255.255",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public_unicast(ip(bad)), "{bad}");
        }
        for good in [
            "1.1.1.1",
            "8.8.8.8",
            "172.15.0.1",
            "172.32.0.1",
            "192.169.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
            "255.255.255.255",
            "0.1.2.3",
        ] {
            assert!(is_public_unicast(ip(good)), "{good}");
        }
    }

    #[tokio::test]
    async fn validates_method_url_and_scheme() {
        let mut r = Router::new();
        register(&mut r);
        let cases = [
            (
                json!({"url": "http://1.1.1.1", "method": "post"}),
                "net.curl: method must be GET or HEAD (got \"POST\")",
            ),
            (json!({}), "net.curl: 'url' is required"),
            (
                json!({"url": "ftp://example.com"}),
                "net.curl: scheme must be http or https (got \"ftp\")",
            ),
            (
                json!({"url": "example.com"}),
                "net.curl: scheme must be http or https (got \"\")",
            ),
            (
                json!({"url": "FILE:///c:/x"}),
                "net.curl: scheme must be http or https (got \"file\")",
            ),
        ];
        for (a, want) in cases {
            let out = r
                .dispatch(Ctx::default(), "net.curl", args(a.clone()))
                .await;
            assert!(!out.ok, "{a}");
            assert_eq!(out.error, want);
        }
        let out = r
            .dispatch(Ctx::default(), "net.curl", args(json!({"url": ":foo"})))
            .await;
        assert_eq!(
            out.error,
            "net.curl: bad url: parse \":foo\": missing protocol scheme"
        );
        let out = r
            .dispatch(
                Ctx::default(),
                "net.curl",
                args(json!({"url": "http://a\u{1}b"})),
            )
            .await;
        assert!(
            out.error.contains("invalid control character in URL"),
            "{}",
            out.error
        );
    }

    #[tokio::test]
    async fn refuses_private_and_loopback_destinations() {
        let mut r = Router::new();
        register(&mut r);
        for (url, ipstr) in [
            ("http://127.0.0.1:9/", "127.0.0.1"),
            ("http://[::1]:9/", "::1"),
            ("http://10.0.0.5/", "10.0.0.5"),
            ("http://192.168.0.1/", "192.168.0.1"),
            ("http://100.64.0.1/", "100.64.0.1"),
        ] {
            let out = r
                .dispatch(Ctx::default(), "net.curl", args(json!({"url": url})))
                .await;
            assert!(!out.ok, "{url}");
            assert_eq!(
                out.error,
                format!("net.curl: refusing private/loopback destination {ipstr}")
            );
        }
        let out = r
            .dispatch(
                Ctx::default(),
                "net.curl",
                args(json!({"url": "http://localhost/"})),
            )
            .await;
        assert!(
            out.error
                .starts_with("net.curl: refusing private/loopback destination ")
                || out.error.starts_with("net.curl: resolve"),
            "{}",
            out.error
        );
    }

    #[tokio::test]
    async fn get_returns_status_headers_and_base64_body() {
        let (port, _) = serve().await;
        let out = fetch(port, "/small", "GET", CLIENT_TIMEOUT).await.unwrap();
        assert_eq!(out["status"], json!(200));
        assert_eq!(out["status_text"], json!("200 OK"));
        assert_eq!(out["body_size"], json!(5));
        assert_eq!(out["body_truncated"], json!(false));
        assert_eq!(body_of(&out), b"hello");
        assert_eq!(out["headers"]["Content-Type"], json!("text/plain"));
        assert_eq!(out["headers"]["X-Multi"], json!("1, 2"));
        assert_eq!(out["headers"]["Content-Length"], json!("5"));
    }

    #[tokio::test]
    async fn head_has_no_body_fields() {
        let (port, _) = serve().await;
        let out = fetch(port, "/small", "HEAD", CLIENT_TIMEOUT).await.unwrap();
        assert_eq!(out["status"], json!(200));
        assert_eq!(out["body_size"], json!(0));
        assert_eq!(out["body_truncated"], json!(false));
        assert!(out.get("body_base64").is_none());
    }

    #[tokio::test]
    async fn bodies_are_capped_at_one_mebibyte() {
        let (port, _) = serve().await;
        let out = fetch(port, "/big", "GET", CLIENT_TIMEOUT).await.unwrap();
        assert_eq!(out["body_size"], json!(BODY_CAP));
        assert_eq!(out["body_truncated"], json!(true));
        assert_eq!(body_of(&out).len(), BODY_CAP);

        let out = fetch(port, "/exact", "GET", CLIENT_TIMEOUT).await.unwrap();
        assert_eq!(out["body_size"], json!(BODY_CAP));
        assert_eq!(out["body_truncated"], json!(false));
    }

    #[tokio::test]
    async fn unknown_status_text_and_404() {
        let (port, _) = serve().await;
        let out = fetch(port, "/nope", "GET", CLIENT_TIMEOUT).await.unwrap();
        assert_eq!(out["status"], json!(404));
        assert_eq!(out["status_text"], json!("404 Not Found"));
        assert_eq!(out["body_size"], json!(0));
        assert_eq!(body_of(&out), b"");
    }

    #[tokio::test]
    async fn redirects_are_followed_with_referer_and_user_agent() {
        let (port, hits) = serve().await;
        let out = fetch(port, "/to-small", "GET", CLIENT_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(body_of(&out), b"hello");
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        let out = fetch(port, "/to-echo", "GET", CLIENT_TIMEOUT)
            .await
            .unwrap();
        let body = String::from_utf8(body_of(&out)).unwrap();
        assert_eq!(
            body,
            format!("GET|http://127.0.0.1:{port}/to-echo|{USER_AGENT_VALUE}")
        );
    }

    #[tokio::test]
    async fn redirect_without_location_is_returned_as_is() {
        let (port, _) = serve().await;
        let out = fetch(port, "/no-location", "GET", CLIENT_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(out["status"], json!(302));
        assert_eq!(out["status_text"], json!("302 Found"));
    }

    #[tokio::test]
    async fn stops_after_five_requests_with_too_many_redirects() {
        let (port, hits) = serve().await;
        let err = fetch(port, "/loop", "GET", CLIENT_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(
            err.message,
            format!("Get \"http://127.0.0.1:{port}/loop\": too many redirects")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 5);
        let err = fetch(port, "/loop", "HEAD", CLIENT_TIMEOUT)
            .await
            .unwrap_err();
        assert!(err.message.starts_with("Head \""), "{}", err.message);
    }

    #[tokio::test]
    async fn redirect_to_another_scheme_is_unsupported() {
        let (port, _) = serve().await;
        let err = fetch(port, "/to-ftp", "GET", CLIENT_TIMEOUT)
            .await
            .unwrap_err();
        assert_eq!(
            err.message,
            "Get \"ftp://127.0.0.1/x\": unsupported protocol scheme \"ftp\""
        );
    }

    #[tokio::test]
    async fn header_wait_is_bounded_by_the_client_timeout() {
        let (port, _) = serve().await;
        let started = std::time::Instant::now();
        let err = fetch(port, "/stall-headers", "GET", Duration::from_millis(600))
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            err.message,
            format!("Get \"http://127.0.0.1:{port}/stall-headers\": context deadline exceeded (Client.Timeout exceeded while awaiting headers)")
        );
    }

    #[tokio::test]
    async fn a_stalled_body_returns_what_arrived() {
        let (port, _) = serve().await;
        let out = fetch(port, "/stall-body", "GET", Duration::from_millis(800))
            .await
            .unwrap();
        assert_eq!(out["status"], json!(200));
        assert_eq!(body_of(&out), b"partial");
        assert_eq!(out["body_size"], json!(7));
        assert_eq!(out["body_truncated"], json!(false));
    }

    #[tokio::test]
    async fn cancellation_aborts_the_request() {
        let (port, _) = serve().await;
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c.cancel();
        });
        let err = run_curl(
            &cancel,
            &format!("http://127.0.0.1:{port}/stall-headers"),
            "GET",
            true,
            CLIENT_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(
            err.message.ends_with(": context canceled"),
            "{}",
            err.message
        );
    }

    #[test]
    fn header_keys_are_canonicalized() {
        assert_eq!(canonical_header_key("content-type"), "Content-Type");
        assert_eq!(canonical_header_key("x-multi"), "X-Multi");
        assert_eq!(canonical_header_key("etag"), "Etag");
    }
}

use super::connect::default_relay;
use super::tunnel::DEFAULT_LOCAL_PORT;
use super::tunnel::PORT_SCAN_RANGE;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelOptions {
    pub token: String,
    pub local_port: u16,
    pub relay: String,
    pub http_host: String,
    pub port_explicit: bool,
}

fn parse_port(raw: &str, shown: &str) -> Result<u16, String> {
    match raw.parse::<i64>().ok().and_then(|p| u16::try_from(p).ok()) {
        Some(p) if p > 0 => Ok(p),
        _ => Err(format!(
            "invalid --local-port {}",
            crate::util::go_quote(shown)
        )),
    }
}

pub fn clean_http_host(host: &str) -> Result<String, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("--http-host requires a non-empty value".to_string());
    }
    if host.contains(['\r', '\n']) {
        return Err("--http-host must not contain newlines".to_string());
    }
    Ok(host.to_string())
}

pub fn parse_tunnel_args(args: &[String]) -> Result<TunnelOptions, String> {
    parse_tunnel_args_with(args, &default_relay())
}

pub fn parse_tunnel_args_with(
    args: &[String],
    default_relay: &str,
) -> Result<TunnelOptions, String> {
    let mut local_port: u16 = 0;
    let mut relay = default_relay.to_string();
    let mut http_host = String::new();
    let mut token = String::new();
    let mut positional = 0;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let value_of = |name: &str, i: &mut usize| -> Result<String, String> {
            if *i + 1 >= args.len() {
                return Err(format!("{name} requires a value"));
            }
            *i += 1;
            Ok(args[*i].clone())
        };
        match a {
            "--local-port" | "-p" => {
                let v = value_of("--local-port", &mut i)?;
                local_port = parse_port(&v, &v)?;
            }
            "--relay" => relay = value_of("--relay", &mut i)?,
            "--http-host" | "--host-header" => {
                let v = value_of("--http-host", &mut i)?;
                http_host = clean_http_host(&v)?;
            }
            _ if a.starts_with("--local-port=") => {
                local_port = parse_port(&a["--local-port=".len()..], a)?;
            }
            _ if a.starts_with("--relay=") => relay = a["--relay=".len()..].to_string(),
            _ if a.starts_with("--http-host=") => {
                http_host = clean_http_host(&a["--http-host=".len()..])?;
            }
            _ if a.starts_with("--host-header=") => {
                http_host = clean_http_host(&a["--host-header=".len()..])?;
            }
            _ if a.starts_with('-') => {
                return Err(format!("unknown flag {}", crate::util::go_quote(a)));
            }
            _ => {
                if positional == 0 {
                    token = a.to_string();
                    positional += 1;
                } else {
                    return Err(format!("unexpected argument {}", crate::util::go_quote(a)));
                }
            }
        }
        i += 1;
    }

    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("connect token is required".to_string());
    }
    let port_explicit = local_port != 0;
    if local_port == 0 {
        local_port = DEFAULT_LOCAL_PORT;
    }
    Ok(TunnelOptions {
        token,
        local_port,
        relay,
        http_host,
        port_explicit,
    })
}

pub fn bind_local_listener(port: u16, explicit: bool) -> std::io::Result<TcpListener> {
    let attempts = if explicit { 1 } else { PORT_SCAN_RANGE };
    let mut last_err: Option<std::io::Error> = None;
    for offset in 0..attempts {
        let Ok(candidate) = u16::try_from(u32::from(port) + offset) else {
            break;
        };
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, candidate));
        match TcpListener::bind(addr) {
            Ok(listener) => return Ok(listener),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no port available")))
}

pub fn is_tunnel_url(token: &str) -> bool {
    ["http://", "https://", "ws://", "wss://"]
        .iter()
        .any(|p| token.starts_with(p))
}

fn swap_scheme(mut u: url::Url) -> url::Url {
    let target = match u.scheme() {
        "http" => Some("ws"),
        "https" => Some("wss"),
        _ => None,
    };
    if let Some(scheme) = target {
        let _ = u.set_scheme(scheme);
    }
    u
}

pub fn tunnel_ws_url(relay_base: &str, token: &str) -> Result<String, String> {
    if is_tunnel_url(token) {
        let u = url::Url::parse(token).map_err(|e| format!("parse the tunnel address: {e}"))?;
        return Ok(swap_scheme(u).to_string());
    }
    let base = relay_base.trim_end_matches('/');
    let u = url::Url::parse(&format!("{base}/api/tunnel/{token}"))
        .map_err(|e| format!("build the tunnel address: {e}"))?;
    Ok(swap_scheme(u).to_string())
}

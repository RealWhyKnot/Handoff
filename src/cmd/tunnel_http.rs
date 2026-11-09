use super::tunnel_args::clean_http_host;
use std::net::IpAddr;

pub const HTTP_HEADER_REWRITE_LIMIT: usize = 64 * 1024;

const HTTP_METHOD_PREFIXES: [&[u8]; 8] = [
    b"GET ",
    b"POST ",
    b"HEAD ",
    b"PUT ",
    b"DELETE ",
    b"OPTIONS ",
    b"PATCH ",
    b"TRACE ",
];

pub fn default_tunnel_http_host(host: &str, port: i64) -> String {
    let host = host.trim();
    if host.is_empty() || is_loopback_tunnel_host(host) {
        return String::new();
    }
    if port <= 0 || port == 80 {
        return clean_http_host(host).unwrap_or_default();
    }
    let host_port = if host.parse::<IpAddr>().is_ok() && host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    clean_http_host(&host_port).unwrap_or_default()
}

fn is_loopback_tunnel_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

pub fn rewrite_http_host_header(data: &[u8], host: &str) -> Option<Vec<u8>> {
    if host.is_empty() {
        return Some(data.to_vec());
    }
    if !has_http_method_prefix(data) {
        if may_become_http_method(data) {
            return None;
        }
        return Some(data.to_vec());
    }
    let Some(header_end) = data.windows(4).position(|w| w == b"\r\n\r\n") else {
        if data.len() < HTTP_HEADER_REWRITE_LIMIT {
            return None;
        }
        return Some(data.to_vec());
    };

    let header = &data[..header_end];
    let body = &data[header_end + 4..];
    let lines = split_crlf(header);

    let host_line = format!("Host: {host}").into_bytes();
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(lines.len() + 1);
    out.push(lines[0].to_vec());
    let mut replaced = false;
    for line in &lines[1..] {
        if line.len() >= 5 && line[..5].eq_ignore_ascii_case(b"host:") {
            if !replaced {
                out.push(host_line.clone());
                replaced = true;
            }
            continue;
        }
        out.push(line.to_vec());
    }
    if !replaced {
        out.insert(1, host_line);
    }

    let mut rewritten = out.join(&b"\r\n"[..]);
    rewritten.extend_from_slice(b"\r\n\r\n");
    rewritten.extend_from_slice(body);
    Some(rewritten)
}

fn split_crlf(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == b'\r' && data[i + 1] == b'\n' {
            out.push(&data[start..i]);
            i += 2;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(&data[start..]);
    out
}

fn has_http_method_prefix(data: &[u8]) -> bool {
    HTTP_METHOD_PREFIXES.iter().any(|m| data.starts_with(m))
}

fn may_become_http_method(data: &[u8]) -> bool {
    if data.is_empty() || data.len() > b"OPTIONS ".len() {
        return false;
    }
    HTTP_METHOD_PREFIXES
        .iter()
        .any(|m| data.len() <= m.len() && m.starts_with(data))
}

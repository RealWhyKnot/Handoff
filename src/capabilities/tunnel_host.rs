use super::tunnel_state::go_io_error;
use crate::util::go_quote;
use futures_util::future::BoxFuture;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

pub const HOST_LOOKUP_TIME: Duration = Duration::from_secs(3);

pub type Lookup = fn(String) -> BoxFuture<'static, Result<Vec<IpAddr>, String>>;

pub fn join_host_port(host: &str, port: i64) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub(super) fn system_lookup(host: String) -> BoxFuture<'static, Result<Vec<IpAddr>, String>> {
    Box::pin(async move {
        match tokio::net::lookup_host((host.as_str(), 0)).await {
            Ok(addrs) => {
                let mut out: Vec<IpAddr> = Vec::new();
                for a in addrs {
                    if !out.contains(&a.ip()) {
                        out.push(a.ip());
                    }
                }
                Ok(out)
            }
            Err(err) => {
                let text = match err.raw_os_error() {
                    Some(11001) | Some(11004) => "no such host".to_string(),
                    _ => go_io_error(&err),
                };
                Err(format!("lookup {host}: {text}"))
            }
        }
    })
}

pub fn split_host_port_ok(hostport: &str) -> bool {
    let Some(i) = hostport.rfind(':') else {
        return false;
    };
    let (j, k);
    if hostport.starts_with('[') {
        let Some(end) = hostport.find(']') else {
            return false;
        };
        if end + 1 == hostport.len() || end + 1 != i {
            return false;
        }
        j = 1;
        k = end + 1;
    } else {
        if hostport[..i].contains(':') {
            return false;
        }
        j = 0;
        k = 0;
    }
    if hostport[j..].contains('[') {
        return false;
    }
    if hostport[k..].contains(']') {
        return false;
    }
    true
}

pub fn check_tunnel_host_text(host: &str) -> Result<(), String> {
    let host = host.trim();
    if host.is_empty() {
        return Ok(());
    }
    if host.contains("://") || host.contains(['/', '\\', '?', '#', '@']) {
        return Err(format!(
            "tunnel.open: host must be a hostname or IP address (got {})",
            go_quote(host)
        ));
    }
    if split_host_port_ok(host) {
        return Err(format!(
            "tunnel.open: host must not include a port (got {})",
            go_quote(host)
        ));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_allowed_tunnel_target_ip(ip) {
            return Err(format!(
                "tunnel.open: host {} is not a local network address",
                go_quote(host)
            ));
        }
        return Ok(());
    }
    if !is_reasonable_tunnel_hostname(host) {
        return Err(format!("tunnel.open: invalid host {}", go_quote(host)));
    }
    Ok(())
}

pub async fn validate_tunnel_target_host(host: &str) -> Result<(String, String), String> {
    validate_tunnel_target_host_with(host, system_lookup).await
}

pub async fn validate_tunnel_target_host_with(
    host: &str,
    lookup: Lookup,
) -> Result<(String, String), String> {
    let host = host.trim();
    check_tunnel_host_text(host)?;
    if host.is_empty() {
        return Ok(("127.0.0.1".to_string(), "127.0.0.1".to_string()));
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok((host.to_string(), host.to_string()));
    }

    let addrs = match tokio::time::timeout(HOST_LOOKUP_TIME, lookup(host.to_string())).await {
        Ok(Ok(a)) => a,
        Ok(Err(err)) => {
            return Err(format!(
                "tunnel.open: resolve host {}: {err}",
                go_quote(host)
            ));
        }
        Err(_) => {
            return Err(format!(
                "tunnel.open: resolve host {}: lookup {host}: i/o timeout",
                go_quote(host)
            ));
        }
    };
    if addrs.is_empty() {
        return Err(format!(
            "tunnel.open: resolve host {}: no addresses",
            go_quote(host)
        ));
    }
    for ip in &addrs {
        if !is_allowed_tunnel_target_ip(*ip) {
            return Err(format!(
                "tunnel.open: host {} resolves outside the local network ({})",
                go_quote(host),
                canonical_ip(*ip)
            ));
        }
    }
    Ok((host.to_string(), canonical_ip(addrs[0]).to_string()))
}

pub fn is_reasonable_tunnel_hostname(host: &str) -> bool {
    if host.len() > 253 {
        return false;
    }
    host.chars()
        .all(|r| r == '.' || r == '-' || r == '_' || r.is_ascii_alphanumeric())
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        v4 @ IpAddr::V4(_) => v4,
    }
}

fn v4_allowed(ip: Ipv4Addr) -> bool {
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    if ip.is_loopback() || ip.is_private() || ip.is_link_local() {
        return true;
    }
    let o = ip.octets();
    o[0] == 100 && o[1] & 0xc0 == 64
}

fn v6_allowed(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    let seg = ip.segments();
    ip.is_loopback() || seg[0] & 0xfe00 == 0xfc00 || seg[0] & 0xffc0 == 0xfe80
}

pub fn is_allowed_tunnel_target_ip(ip: IpAddr) -> bool {
    match canonical_ip(ip) {
        IpAddr::V4(v4) => v4_allowed(v4),
        IpAddr::V6(v6) => v6_allowed(v6),
    }
}

fn is_loopback_ip_text(host: &str) -> bool {
    host.parse::<IpAddr>()
        .map(|ip| canonical_ip(ip).is_loopback())
        .unwrap_or(false)
}

pub fn tunnel_target_description(host_arg: &str, port: i64) -> String {
    let mut host = host_arg.trim();
    if host.is_empty() {
        host = "127.0.0.1";
    }
    if host.eq_ignore_ascii_case("localhost") || is_loopback_ip_text(host) {
        return format!("a service running on this computer ({host}:{port})");
    }
    format!(
        "a device on this computer's local network, {host}:{port} (for example a router or printer)"
    )
}

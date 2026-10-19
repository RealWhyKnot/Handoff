use std::net::IpAddr;
use std::time::Duration;

use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router, Spec};

use super::net::arg_string;
use super::net_probes::valid_target;

pub fn register(r: &mut Router) {
    let mut specs: Vec<Spec> = serde_json::from_str(include_str!("specs/net_resolve.json"))
        .expect("embedded capability specs are valid JSON");
    let spec = specs.remove(0);
    r.register_spec(spec, handler(net_resolve));
}

pub fn go_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        other => other,
    }
}

pub fn ip_string(ip: IpAddr) -> String {
    go_ip(ip).to_string()
}

pub fn is_v4(ip: IpAddr) -> bool {
    go_ip(ip).is_ipv4()
}

fn lookup_error_text(err: &std::io::Error) -> String {
    match err.raw_os_error() {
        Some(11001) | Some(11004) | Some(9003) => "no such host".to_string(),
        Some(_) => {
            let text = err.to_string();
            let text = match text.rfind(" (os error ") {
                Some(i) => text[..i].to_string(),
                None => text,
            };
            format!("getaddrinfow: {text}")
        }
        None => "no such host".to_string(),
    }
}

pub async fn lookup_ip_addrs(
    cancel: &CancellationToken,
    host: &str,
    deadline: Option<Instant>,
) -> Result<Vec<IpAddr>, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    if host.is_empty() {
        return Err("lookup : no such host".to_string());
    }
    let name = host.to_string();
    let lookup = async {
        tokio::net::lookup_host((name.as_str(), 0u16))
            .await
            .map(|it| it.map(|sa| sa.ip()).collect::<Vec<_>>())
    };
    let timed = async {
        match deadline {
            Some(d) => tokio::time::timeout_at(d, lookup).await.ok(),
            None => Some(lookup.await),
        }
    };
    tokio::select! {
        _ = cancel.cancelled() => Err(format!("lookup {host}: operation was canceled")),
        r = timed => match r {
            None => Err(format!("lookup {host}: i/o timeout")),
            Some(Ok(addrs)) => Ok(addrs),
            Some(Err(e)) => Err(format!("lookup {host}: {}", lookup_error_text(&e))),
        },
    }
}

#[cfg(windows)]
mod dns {
    use std::ffi::c_void;

    #[repr(C)]
    struct DnsRecord {
        next: *mut DnsRecord,
        name: *mut u16,
        rtype: u16,
        data_len: u16,
        flags: u32,
        ttl: u32,
        reserved: u32,
        name_host: *mut u16,
    }

    #[link(name = "dnsapi")]
    extern "system" {
        fn DnsQuery_W(
            name: *const u16,
            wtype: u16,
            options: u32,
            extra: *mut c_void,
            results: *mut *mut DnsRecord,
            reserved: *mut *mut c_void,
        ) -> i32;
        fn DnsRecordListFree(list: *mut DnsRecord, free_type: i32);
    }

    const DNS_TYPE_CNAME: u16 = 5;
    const DNS_INFO_NO_RECORDS: i32 = 9501;

    unsafe fn wide_to_string(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }

    fn same_name(a: &str, b: &str) -> bool {
        a.trim_end_matches('.')
            .eq_ignore_ascii_case(b.trim_end_matches('.'))
    }

    pub fn lookup_cname(name: &str) -> String {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut rec: *mut DnsRecord = std::ptr::null_mut();
        let status = unsafe {
            DnsQuery_W(
                wide.as_ptr(),
                DNS_TYPE_CNAME,
                0,
                std::ptr::null_mut(),
                &mut rec,
                std::ptr::null_mut(),
            )
        };
        if status == DNS_INFO_NO_RECORDS {
            return name.trim_end_matches('.').to_string();
        }
        if status != 0 {
            return String::new();
        }
        let mut current = name.to_string();
        unsafe {
            for _ in 0..10 {
                let mut next_name: Option<String> = None;
                let mut p = rec;
                while !p.is_null() {
                    let r = &*p;
                    if r.flags & 3 == 1
                        && r.rtype == DNS_TYPE_CNAME
                        && same_name(&current, &wide_to_string(r.name))
                    {
                        next_name = Some(wide_to_string(r.name_host));
                        break;
                    }
                    p = r.next;
                }
                match next_name {
                    Some(n) => current = n,
                    None => break,
                }
            }
            DnsRecordListFree(rec, 1);
        }
        current.trim_end_matches('.').to_string()
    }
}

#[cfg(not(windows))]
mod dns {
    pub fn lookup_cname(name: &str) -> String {
        name.trim_end_matches('.').to_string()
    }
}

pub async fn lookup_cname(cancel: &CancellationToken, name: &str, deadline: Instant) -> String {
    let n = name.to_string();
    let work = tokio::task::spawn_blocking(move || dns::lookup_cname(&n));
    tokio::select! {
        _ = cancel.cancelled() => String::new(),
        r = tokio::time::timeout_at(deadline, work) => match r {
            Ok(Ok(s)) => s,
            _ => String::new(),
        },
    }
}

async fn net_resolve(ctx: Ctx, args: Args) -> CapResult {
    let name = arg_string(&args, "name").unwrap_or_default();
    let want = arg_string(&args, "type").unwrap_or_default();
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(HandlerError::plain("net.resolve: 'name' is required"));
    }
    if name.len() > 253 {
        return Err(HandlerError::plain("net.resolve: 'name' is too long"));
    }
    if !valid_target(&name) {
        return Err(HandlerError::plain(
            "net.resolve: 'name' contains characters that are not valid in a hostname",
        ));
    }
    let want = if want.is_empty() {
        "any".to_string()
    } else {
        want
    };

    let started = std::time::Instant::now();
    let deadline = Instant::now() + Duration::from_secs(10);
    let looked_up = lookup_ip_addrs(&ctx.cancel, &name, Some(deadline)).await;
    let elapsed = started.elapsed().as_millis() as i64;
    let addrs = match looked_up {
        Ok(a) => a,
        Err(e) => {
            return Ok(json!({
                "name": name,
                "type": want,
                "resolved": false,
                "addresses": Vec::<String>::new(),
                "error": e,
                "elapsed_ms": elapsed,
            }));
        }
    };

    let mut out: Vec<String> = Vec::with_capacity(addrs.len());
    for a in addrs {
        let v4 = is_v4(a);
        match want.as_str() {
            "a" if !v4 => continue,
            "aaaa" if v4 => continue,
            _ => {}
        }
        out.push(ip_string(a));
    }

    let cname = lookup_cname(&ctx.cancel, &name, deadline).await;
    Ok(json!({
        "name": name,
        "type": want,
        "resolved": !out.is_empty(),
        "addresses": out,
        "cname": cname,
        "elapsed_ms": elapsed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::Args;
    use serde_json::Value;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    async fn run(a: Value) -> crate::dispatch::Outcome {
        let mut r = Router::new();
        register(&mut r);
        r.dispatch(Ctx::default(), "net.resolve", args(a)).await
    }

    #[test]
    fn registers_the_spec_from_the_hello_fixture() {
        let mut r = Router::new();
        register(&mut r);
        let spec = r.spec_for("net.resolve").unwrap();
        assert_eq!(spec.label, "Resolve name");
        assert_eq!(spec.params.len(), 2);
    }

    #[tokio::test]
    async fn net_resolve_localhost() {
        let out = run(json!({"name": "localhost"})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["resolved"], json!(true), "{}", out.result);
        assert!(!out.result["addresses"].as_array().unwrap().is_empty());
        assert_eq!(out.result["type"], json!("any"));
    }

    #[tokio::test]
    async fn net_resolve_filters_by_family() {
        let out = run(json!({"name": "127.0.0.1", "type": "aaaa"})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["resolved"], json!(false));
        assert_eq!(out.result["addresses"], json!([]));
        let out = run(json!({"name": "127.0.0.1", "type": "a"})).await;
        assert_eq!(out.result["addresses"], json!(["127.0.0.1"]));
        assert!(out.result["cname"].is_string());
    }

    #[tokio::test]
    async fn net_resolve_reports_failure_without_erroring() {
        let out = run(json!({"name": "this-name-should-not-exist.invalid"})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["resolved"], json!(false), "{}", out.result);
        assert!(out.result["error"]
            .as_str()
            .unwrap()
            .starts_with("lookup this-name-should-not-exist.invalid: "));
        assert_eq!(out.result["addresses"], json!([]));
        assert!(out.result.get("cname").is_none());
    }

    #[tokio::test]
    async fn net_resolve_validates_name() {
        for bad in ["".to_string(), "a".repeat(300), "bad host;name".to_string()] {
            let out = run(json!({"name": bad})).await;
            assert!(!out.ok, "name {bad:?} was accepted");
        }
    }

    #[tokio::test]
    async fn net_resolve_rejects_an_unknown_type() {
        let out = run(json!({"name": "localhost", "type": "mx"})).await;
        assert!(!out.ok);
        assert!(out.error.contains("must be one of"), "{}", out.error);
    }

    #[test]
    fn mapped_v6_addresses_print_as_v4() {
        let ip: IpAddr = "::ffff:1.2.3.4".parse().unwrap();
        assert_eq!(ip_string(ip), "1.2.3.4");
        assert!(is_v4(ip));
        let ip: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(ip_string(ip), "2001:db8::1");
        assert!(!is_v4(ip));
    }
}

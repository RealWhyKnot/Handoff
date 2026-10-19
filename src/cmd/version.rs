use std::cmp::Ordering;

use serde_json::{json, Value};

use super::flags::parse_options;
use super::paths::{audit_dir, support_log_path};
use crate::util;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedVersion {
    pub parts: [u64; 4],
    pub suffix: String,
}

pub fn parse_version(input: &str) -> Option<ParsedVersion> {
    let s = input.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    let (number, suffix) = match s.split_once('-') {
        Some((n, sfx)) => (n, sfx.to_string()),
        None => (s, String::new()),
    };
    let fields: Vec<&str> = number.split('.').collect();
    if fields.len() != 4 {
        return None;
    }
    let mut parts = [0u64; 4];
    for (slot, field) in parts.iter_mut().zip(&fields) {
        let digits = field.strip_prefix('+').unwrap_or(field);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = digits.parse().ok()?;
    }
    Some(ParsedVersion { parts, suffix })
}

pub fn compare_versions(a: &ParsedVersion, b: &ParsedVersion) -> i32 {
    match a.parts.cmp(&b.parts) {
        Ordering::Less => return -1,
        Ordering::Greater => return 1,
        Ordering::Equal => {}
    }
    if a.suffix == b.suffix {
        0
    } else if a.suffix.is_empty() {
        1
    } else if b.suffix.is_empty() || a.suffix < b.suffix {
        -1
    } else {
        1
    }
}

pub fn print_version(args: &[String], version: &str) -> i32 {
    let (opts, _) = match parse_options("version", args, &[]) {
        Ok(v) => v,
        Err(_) => return 2,
    };

    let info = json!({
        "version": version,
        "relay": opts.relay,
        "log_path": support_log_path(Some(&opts)),
        "audit_dir": audit_dir(Some(&opts)),
    });

    if opts.json {
        println!("{}", util::go_pretty_json(&info));
        return 0;
    }

    let field = |key: &str| info[key].as_str().unwrap_or("").to_string();
    println!("handoff {version}");
    println!("relay:  {}", field("relay"));
    println!("log:    {}", field("log_path"));
    println!("audit:  {}", field("audit_dir"));
    0
}

pub fn version_info(version: &str, relay: &str, log_path: &str, audit_dir: &str) -> Value {
    json!({
        "version": version,
        "relay": relay,
        "log_path": log_path,
        "audit_dir": audit_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_cases() {
        type Case<'a> = (&'a str, Option<([u64; 4], &'a str)>);
        let cases: [Case; 8] = [
            ("2026.8.18.0", Some(([2026, 8, 18, 0], ""))),
            ("v2026.8.18.0", Some(([2026, 8, 18, 0], ""))),
            ("2026.8.18.0-22EE", Some(([2026, 8, 18, 0], "22EE"))),
            ("2026.12.1.7", Some(([2026, 12, 1, 7], ""))),
            ("dev", None),
            ("", None),
            ("2026.8.18", None),
            ("2026.8.18.x", None),
        ];
        for (input, want) in cases {
            let got = parse_version(input);
            match want {
                None => assert!(got.is_none(), "{input:?} should not parse"),
                Some((parts, suffix)) => {
                    let got = got.unwrap_or_else(|| panic!("{input:?} should parse"));
                    assert_eq!(got.parts, parts, "{input}");
                    assert_eq!(got.suffix, suffix, "{input}");
                }
            }
        }
    }

    #[test]
    fn compare_versions_orders_numerically_and_suffix_first() {
        let cases = [
            ("2026.8.18.0", "2026.8.18.0", 0),
            ("2026.8.18.0", "2026.8.18.1", -1),
            ("2026.8.18.1", "2026.8.18.0", 1),
            ("2026.8.9.0", "2026.8.18.0", -1),
            ("2025.12.31.9", "2026.1.1.0", -1),
            ("2026.8.18.0-22EE", "2026.8.18.0", -1),
            ("2026.8.18.0", "2026.8.18.0-22EE", 1),
        ];
        for (a, b, want) in cases {
            let pa = parse_version(a).unwrap();
            let pb = parse_version(b).unwrap();
            assert_eq!(compare_versions(&pa, &pb), want, "{a} vs {b}");
        }
    }

    #[test]
    fn version_info_keys_come_out_sorted() {
        let info = version_info("dev", "https://r.example", "log", "audit");
        let text = serde_json::to_string(&info).unwrap();
        assert_eq!(
            text,
            r#"{"audit_dir":"audit","log_path":"log","relay":"https://r.example","version":"dev"}"#
        );
    }
}

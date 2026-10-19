use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::capabilities::pwsh::normalize_numbers;
use crate::console;
use crate::dispatch::Outcome;
use crate::util;

const SUMMARY_KEYS: [&str; 15] = [
    "path",
    "target",
    "url",
    "host",
    "name",
    "action",
    "pid",
    "query",
    "script",
    "channel",
    "serial",
    "local_port",
    "key",
    "uf2_path",
    "out_path",
];

const BULK_KEYS: [&str; 5] = [
    "data_base64",
    "body_base64",
    "base64",
    "image_base64",
    "zip_base64",
];

const MAX_SUMMARY_VALUE: usize = 120;
const MAX_SUMMARY_TOTAL: usize = 200;

fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn summarize_args(args: &Map<String, Value>) -> String {
    if args.is_empty() {
        return String::new();
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut parts: Vec<String> = Vec::new();

    let mut add = |key: &str, parts: &mut Vec<String>| {
        let Some(value) = args.get(key) else {
            return;
        };
        if !seen.insert(key.to_string()) {
            return;
        }
        if BULK_KEYS.contains(&key) {
            let raw = serde_json::to_string(value).unwrap_or_default();
            parts.push(format!("{key}=<{} bytes>", raw.len()));
            return;
        }
        parts.push(format!("{key}={}", summary_value(value)));
    };

    for key in SUMMARY_KEYS {
        add(key, &mut parts);
    }
    if parts.is_empty() {
        let mut rest: Vec<&String> = args.keys().collect();
        rest.sort();
        for key in rest {
            add(key, &mut parts);
            if parts.len() >= 2 {
                break;
            }
        }
    }

    let out = parts.join(" ");
    if out.len() > MAX_SUMMARY_TOTAL {
        return format!("{}...", truncate_bytes(&out, MAX_SUMMARY_TOTAL));
    }
    out
}

fn summary_value(value: &Value) -> String {
    let mut s = match value {
        Value::String(s) => s.clone(),
        other => util::go_json_escape_html(
            &serde_json::to_string(&normalize_numbers(other.clone())).unwrap_or_default(),
        ),
    };
    s = s.replace('\n', " ").replace('\r', "");
    if s.len() > MAX_SUMMARY_VALUE {
        s = format!("{}...", truncate_bytes(&s, MAX_SUMMARY_VALUE));
    }
    if s.contains([' ', '\t']) {
        return util::go_quote(&s);
    }
    s
}

const NAME_KEYS: [&str; 8] = [
    "Name", "name", "Caption", "Model", "Product", "hostname", "title", "path",
];

fn cut(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() > MAX_SUMMARY_VALUE {
        return format!("{}...", truncate_bytes(&text, MAX_SUMMARY_VALUE));
    }
    text
}

fn items(n: usize) -> String {
    console::plural(n, "item", "items")
}

pub fn summarize_result(out: &Outcome) -> String {
    if !out.ok {
        return cut(&console::plain_error(&out.error));
    }

    match &out.result {
        Value::Object(res) => {
            if let Some(code) = res.get("exit_code") {
                let printed = res
                    .get("stdout")
                    .and_then(Value::as_str)
                    .map(str::len)
                    .unwrap_or(0);
                return format!(
                    "Exit code {}, {} of output",
                    util::go_value_string(code),
                    console::human_bytes(printed as u64)
                );
            }
            if let Some(n) = res.get("count").and_then(Value::as_u64) {
                return items(n as usize);
            }
            if let Some(size) = res.get("size").and_then(Value::as_u64) {
                return console::human_bytes(size);
            }
            if let Some(Value::Array(entries)) = res.get("entries") {
                return items(entries.len());
            }
            for key in NAME_KEYS {
                if let Some(Value::String(name)) = res.get(key) {
                    if !name.trim().is_empty() {
                        return cut(name);
                    }
                }
            }
            String::new()
        }
        Value::Array(list) => items(list.len()),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn ok(result: Value) -> Outcome {
        Outcome {
            ok: true,
            result,
            ..Outcome::default()
        }
    }

    #[test]
    fn empty_arguments_have_no_summary() {
        assert_eq!(summarize_args(&Map::new()), "");
    }

    #[test]
    fn interesting_keys_come_first_in_a_fixed_order() {
        let a = args(json!({"limit": 5, "path": "C:\\Temp", "name": "x"}));
        assert_eq!(summarize_args(&a), "path=C:\\Temp name=x");
    }

    #[test]
    fn values_with_spaces_are_quoted() {
        let a = args(json!({"path": "C:\\Program Files"}));
        assert_eq!(summarize_args(&a), "path=\"C:\\\\Program Files\"");
    }

    #[test]
    fn bulk_payloads_show_their_size_not_their_content() {
        let a = args(json!({"path": "f", "data_base64": "QUJD"}));
        assert_eq!(summarize_args(&a), "path=f");
        let a = args(json!({"data_base64": "QUJD"}));
        assert_eq!(summarize_args(&a), "data_base64=<6 bytes>");
    }

    #[test]
    fn falls_back_to_the_first_two_sorted_keys() {
        let a = args(json!({"zeta": 1, "alpha": true, "mid": "m"}));
        assert_eq!(summarize_args(&a), "alpha=true mid=m");
    }

    #[test]
    fn long_values_and_totals_are_cut() {
        let long = "a".repeat(300);
        let a = args(json!({"path": long}));
        let s = summarize_args(&a);
        assert_eq!(s, format!("path={}...", "a".repeat(120)));

        let a = args(json!({
            "path": "p".repeat(120),
            "target": "t".repeat(120),
        }));
        let s = summarize_args(&a);
        assert!(s.ends_with("..."));
        assert_eq!(s.len(), 203);
    }

    #[test]
    fn newlines_are_flattened() {
        let a = args(json!({"script": "a\r\nb"}));
        assert_eq!(summarize_args(&a), "script=\"a b\"");
    }

    #[test]
    fn non_string_values_print_as_json() {
        let a = args(json!({"pid": 42, "recursive": [1, 2]}));
        assert_eq!(summarize_args(&a), "pid=42");
        let a = args(json!({"recursive": [1, 2]}));
        assert_eq!(summarize_args(&a), "recursive=[1,2]");
    }

    #[test]
    fn results_are_described_by_shape() {
        assert_eq!(
            summarize_result(&ok(json!({"exit_code": 0, "stdout": "hello"}))),
            "Exit code 0, 5 bytes of output"
        );
        assert_eq!(summarize_result(&ok(json!({"count": 3}))), "3 items");
        assert_eq!(summarize_result(&ok(json!({"count": 1}))), "1 item");
        assert_eq!(summarize_result(&ok(json!({"size": 2048}))), "2.0 KB");
        assert_eq!(summarize_result(&ok(json!({"entries": [1, 2]}))), "2 items");
        assert_eq!(summarize_result(&ok(json!([1, 2, 3]))), "3 items");
        assert_eq!(summarize_result(&ok(json!({"other": 1}))), "");
        assert_eq!(summarize_result(&ok(json!("text"))), "");
    }

    #[test]
    fn a_name_field_is_shown_when_nothing_else_fits() {
        assert_eq!(
            summarize_result(&ok(json!({
                "Manufacturer": "AuthenticAMD",
                "Name": "AMD Ryzen 7 5800X3D 8-Core Processor           ",
                "NumberOfCores": 8
            }))),
            "AMD Ryzen 7 5800X3D 8-Core Processor"
        );
    }

    #[test]
    fn failures_report_the_error_on_one_line() {
        let out = Outcome {
            ok: false,
            error: "line one
line two"
                .into(),
            ..Outcome::default()
        };
        assert_eq!(summarize_result(&out), "line one line two");
        let out = Outcome {
            ok: false,
            error: "e".repeat(200),
            ..Outcome::default()
        };
        assert_eq!(summarize_result(&out), format!("{}...", "e".repeat(120)));
        let out = Outcome {
            ok: false,
            error: "Post x: context deadline exceeded".into(),
            ..Outcome::default()
        };
        assert_eq!(summarize_result(&out), "Post x: it took too long");
    }
}

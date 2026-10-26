use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::layout;
use crate::capabilities::pwsh::normalize_numbers;
use crate::console;
use crate::dispatch::Outcome;
use crate::util;

const SUMMARY_KEYS: [&str; 16] = [
    "path",
    "target",
    "url",
    "host",
    "name",
    "action",
    "pid",
    "query",
    "script",
    "command",
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

pub const BODY_KEYS: [&str; 2] = ["script", "command"];

const MAX_SUMMARY_VALUE: usize = 120;
const MAX_SUMMARY_TOTAL: usize = 200;
const MAX_INLINE_VALUE: usize = 48;
const MAX_ERROR: usize = 600;

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArgView {
    pub inline: Vec<String>,
    pub blocks: Vec<(String, String)>,
}

pub fn describe_args(args: &Map<String, Value>) -> ArgView {
    let mut view = ArgView::default();
    if args.is_empty() {
        return view;
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut total = 0usize;
    let mut cut_short = false;

    let mut add = |key: &str, view: &mut ArgView| -> bool {
        if key == "consent_minutes" {
            return false;
        }
        let Some(value) = args.get(key) else {
            return false;
        };
        if !seen.insert(key.to_string()) {
            return false;
        }
        if BULK_KEYS.contains(&key) {
            let raw = serde_json::to_string(value).unwrap_or_default();
            view.inline.push(format!("{key}=<{} bytes>", raw.len()));
            return true;
        }
        if let Value::String(text) = value {
            if goes_in_block(text) {
                view.blocks.push((key.to_string(), text.clone()));
                return true;
            }
        }
        let part = format!("{key}={}", summary_value(value));
        if total + part.len() > MAX_SUMMARY_TOTAL {
            cut_short = true;
            return true;
        }
        total += part.len() + 1;
        view.inline.push(part);
        true
    };

    for key in SUMMARY_KEYS {
        add(key, &mut view);
    }
    if view.inline.is_empty() && view.blocks.is_empty() {
        let mut rest: Vec<&String> = args.keys().collect();
        rest.sort();
        let mut taken = 0;
        for key in rest {
            if add(key, &mut view) {
                taken += 1;
            }
            if taken >= 2 {
                break;
            }
        }
    }
    if cut_short {
        view.inline.push("...".to_string());
    }
    view
}

fn goes_in_block(text: &str) -> bool {
    layout::clean_lines(text).len() > 1
        || console::display_width(&layout::clean_inline(text)) > MAX_INLINE_VALUE
}

fn summary_value(value: &Value) -> String {
    let s = match value {
        Value::String(s) => layout::clean_inline(s),
        other => util::go_json_escape_html(
            &serde_json::to_string(&normalize_numbers(other.clone())).unwrap_or_default(),
        ),
    };
    if s.contains([' ', '\t']) {
        return util::go_quote(&s);
    }
    s
}

const NAME_KEYS: [&str; 8] = [
    "Name", "name", "Caption", "Model", "Product", "hostname", "title", "path",
];

fn cut(text: &str) -> String {
    cut_at(text, MAX_SUMMARY_VALUE)
}

fn cut_at(text: &str, max: usize) -> String {
    let text = layout::clean_inline(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.len() > max {
        return format!("{}...", truncate_bytes(&text, max));
    }
    text
}

fn items(n: usize) -> String {
    console::plural(n, "item", "items")
}

pub fn summarize_result(out: &Outcome) -> String {
    if !out.ok {
        return cut_at(&console::plain_error(&out.error), MAX_ERROR);
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

    fn inline(v: Value) -> Vec<String> {
        describe_args(&args(v)).inline
    }

    #[test]
    fn empty_arguments_have_no_summary() {
        assert_eq!(describe_args(&Map::new()), ArgView::default());
    }

    #[test]
    fn interesting_keys_come_first_in_a_fixed_order() {
        assert_eq!(
            inline(json!({"limit": 5, "path": "C:\\Temp", "name": "x"})),
            ["path=C:\\Temp", "name=x"]
        );
    }

    #[test]
    fn short_values_with_spaces_are_quoted() {
        assert_eq!(
            inline(json!({"path": "C:\\Program Files"})),
            ["path=\"C:\\\\Program Files\""]
        );
    }

    #[test]
    fn bulk_payloads_show_their_size_not_their_content() {
        assert_eq!(
            inline(json!({"path": "f", "data_base64": "QUJD"})),
            ["path=f"]
        );
        assert_eq!(
            inline(json!({"data_base64": "QUJD"})),
            ["data_base64=<6 bytes>"]
        );
    }

    #[test]
    fn falls_back_to_the_first_two_sorted_keys() {
        assert_eq!(
            inline(json!({"zeta": 1, "alpha": true, "mid": "m"})),
            ["alpha=true", "mid=m"]
        );
    }

    #[test]
    fn long_values_move_to_their_own_block_uncut() {
        let long = "a".repeat(300);
        let view = describe_args(&args(json!({"path": long, "pid": 4})));
        assert_eq!(view.inline, ["pid=4"]);
        assert_eq!(view.blocks, [("path".to_string(), "a".repeat(300))]);
    }

    #[test]
    fn short_scripts_stay_on_the_header() {
        assert_eq!(inline(json!({"script": "Get-Date"})), ["script=Get-Date"]);
        assert_eq!(
            inline(json!({"command": "Get-Process | Sort CPU"})),
            ["command=\"Get-Process | Sort CPU\""]
        );
    }

    #[test]
    fn multi_line_values_become_blocks() {
        let view = describe_args(&args(json!({"script": "a\r\nb"})));
        assert!(view.inline.is_empty());
        assert_eq!(view.blocks, [("script".to_string(), "a\r\nb".to_string())]);
        let view = describe_args(&args(json!({"script": "\r\nGet-Date\r\n"})));
        assert_eq!(view.inline, ["script=Get-Date"]);
    }

    #[test]
    fn inline_values_lose_escape_sequences_and_hidden_characters() {
        assert_eq!(
            inline(json!({"path": "C:\\x\u{202E}y\u{1b}[31mz\u{7}"})),
            ["path=C:\\xyz"]
        );
    }

    #[test]
    fn the_header_total_is_capped() {
        let v = "v".repeat(45);
        let parts = inline(json!({
            "path": v, "target": v, "url": v, "host": v, "name": v, "query": v,
        }));
        assert_eq!(parts.last().map(String::as_str), Some("..."));
        let total: usize = parts.iter().map(|p| p.len() + 1).sum();
        assert!(total <= MAX_SUMMARY_TOTAL + 4);
    }

    #[test]
    fn the_requested_grant_length_is_not_listed() {
        assert_eq!(
            inline(json!({"consent_minutes": 30, "pid": 42})),
            ["pid=42"]
        );
        assert!(inline(json!({"consent_minutes": 30})).is_empty());
    }

    #[test]
    fn non_string_values_print_as_json() {
        assert_eq!(inline(json!({"pid": 42, "recursive": [1, 2]})), ["pid=42"]);
        assert_eq!(inline(json!({"recursive": [1, 2]})), ["recursive=[1,2]"]);
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
        assert_eq!(summarize_result(&out), "e".repeat(200));
        let out = Outcome {
            ok: false,
            error: "e".repeat(700),
            ..Outcome::default()
        };
        assert_eq!(summarize_result(&out), format!("{}...", "e".repeat(600)));
        let out = Outcome {
            ok: false,
            error: "Post x: context deadline exceeded".into(),
            ..Outcome::default()
        };
        assert_eq!(summarize_result(&out), "Post x: it took too long");
    }
}

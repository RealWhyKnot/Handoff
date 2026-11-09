use super::exec::Reply;
use crate::console::{self, plain_error};
use crate::util;
use serde_json::Value;

pub(super) const MAX_CELL: usize = 60;

pub(super) fn reply_field(reply: &Reply, key: &str) -> String {
    match reply.get(key) {
        Some(v) => util::go_value_string(v),
        None => "<nil>".to_string(),
    }
}

pub(super) fn print_exec_result(reply: &Reply, ok: bool) {
    if !ok {
        crate::errln!(
            "command failed: {}",
            plain_error(&reply_field(reply, "error"))
        );
        if let Some(detail) = reply.get("detail")
            && !detail.is_null()
        {
            crate::errln!("{}", util::go_pretty_json(detail));
        }
        return;
    }
    match reply.get("result") {
        None | Some(Value::Null) => crate::outln!("ok"),
        Some(Value::String(s)) => crate::outln!("{s}"),
        Some(other) => match render_table(other) {
            Some(text) => crate::outln!("{text}"),
            None => crate::outln!("{}", util::go_pretty_json(other)),
        },
    }
}

pub(super) fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.trim().to_string(),
        Value::Number(_) | Value::Bool(_) => util::go_value_string(v),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn rows_of(list: &[Value]) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    if list.is_empty() {
        return None;
    }
    let mut headers: Vec<String> = Vec::new();
    for item in list {
        match item {
            Value::Object(map) => {
                for key in map.keys() {
                    if !headers.contains(key) {
                        headers.push(key.clone());
                    }
                }
            }
            Value::Null | Value::Array(_) => return None,
            _ => {}
        }
    }
    if headers.is_empty() {
        let rows = list.iter().map(|v| vec![cell(v)]).collect();
        return Some((vec!["Value".to_string()], rows));
    }
    let rows = list
        .iter()
        .map(|item| match item {
            Value::Object(map) => headers
                .iter()
                .map(|h| map.get(h).map(cell).unwrap_or_default())
                .collect(),
            other => {
                let mut row = vec![cell(other)];
                row.resize(headers.len(), String::new());
                row
            }
        })
        .collect();
    Some((headers, rows))
}

pub fn render_table(result: &Value) -> Option<String> {
    match result {
        Value::Array(list) => {
            let (headers, rows) = rows_of(list)?;
            Some(console::table(&headers, &rows, MAX_CELL))
        }
        Value::Object(map) => {
            let lists: Vec<(&String, &Vec<Value>)> = map
                .iter()
                .filter_map(|(k, v)| match v {
                    Value::Array(list) if list.iter().any(Value::is_object) => Some((k, list)),
                    _ => None,
                })
                .collect();
            if lists.len() != 1 {
                return None;
            }
            let (name, list) = lists[0];
            if map
                .iter()
                .any(|(k, v)| k != name && (v.is_object() || v.is_array()))
            {
                return None;
            }
            let (headers, rows) = rows_of(list)?;
            let mut out = Vec::new();
            let width = map.keys().map(|k| k.len()).max().unwrap_or(0);
            for (k, v) in map {
                if k != name {
                    out.push(format!("{}  {}", console::pad(k, width), cell(v)));
                }
            }
            if !out.is_empty() {
                out.push(String::new());
            }
            out.push(format!("{name}:"));
            out.push(console::table(&headers, &rows, MAX_CELL));
            Some(out.join("\n"))
        }
        _ => None,
    }
}

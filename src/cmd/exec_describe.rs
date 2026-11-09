use super::exec::EXIT_USAGE;
use super::exec_relay::client;
use super::exec_relay::remaining;
use super::exec_render::MAX_CELL;
use super::exec_render::cell;
use crate::capabilities::{self};
use crate::consent::{self, category_for};
use crate::console::{self, human_duration, plain_error};
use crate::dispatch::{ParamType, Router, Spec};
use crate::util;
use serde_json::Value;
use std::time::{Duration, Instant};

pub(super) fn local_specs() -> Vec<Spec> {
    let mut router = Router::new();
    capabilities::register_all(&mut router, None);
    let mut specs = router.specs();
    specs.sort_by(|a, b| a.kind.cmp(&b.kind));
    specs
}

pub(super) fn describe_command(kind: &str) -> i32 {
    let specs = local_specs();
    match specs.iter().find(|s| s.kind == kind) {
        Some(spec) => {
            crate::outln!("{}", describe_spec(spec));
            0
        }
        None => {
            let close: Vec<&str> = specs
                .iter()
                .map(|s| s.kind.as_str())
                .filter(|k| {
                    let group = kind.split('.').next().unwrap_or(kind);
                    k.starts_with(&format!("{group}."))
                })
                .collect();
            if close.is_empty() {
                crate::errln!("There's no command called {kind}.");
            } else {
                crate::errln!(
                    "There's no command called {kind}. Similar ones: {}",
                    close.join(", ")
                );
            }
            EXIT_USAGE
        }
    }
}

fn param_type(kind: &ParamType) -> &'static str {
    match kind {
        ParamType::String => "text",
        ParamType::Int => "number",
        ParamType::Bool => "true/false",
        ParamType::Enum => "choice",
        ParamType::BytesBase64 => "file data",
    }
}

pub fn describe_spec(spec: &Spec) -> String {
    let mut out = Vec::new();
    let title = if spec.label.is_empty() {
        spec.kind.clone()
    } else {
        format!("{} ({})", spec.label, spec.kind)
    };
    out.push(console::paint(console::Tone::Bold, &title));
    if !spec.description.is_empty() {
        out.push(spec.description.clone());
    }
    out.push(String::new());
    let category = category_for(&spec.kind);
    match category {
        Some(c) => out.push(format!(
            "This changes the computer. Handoff asks the person at the computer first. A yes lets the helper {} {}.",
            c.label(),
            consent::scope_text(Some(c))
        )),
        None if spec.risky => out.push("This can change the computer.".to_string()),
        None => out.push("This only reads information. It doesn't change anything.".to_string()),
    }
    if spec.default_timeout_ms > 0 {
        out.push(format!(
            "It stops on its own after {}.",
            human_duration(Duration::from_millis(u64::from(spec.default_timeout_ms)))
        ));
    }
    out.push(String::new());
    if spec.params.is_empty() {
        out.push("It takes no arguments.".to_string());
        return out.join("\n");
    }
    let headers: Vec<String> = [
        "Argument", "Type", "Required", "Default", "Allowed", "About",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let rows: Vec<Vec<String>> = spec
        .params
        .iter()
        .map(|p| {
            let allowed = if !p.enum_values.is_empty() {
                p.enum_values.join(", ")
            } else {
                match (p.min, p.max) {
                    (Some(lo), Some(hi)) => format!("{lo} to {hi}"),
                    (Some(lo), None) => format!("{lo} or more"),
                    (None, Some(hi)) => format!("up to {hi}"),
                    (None, None) => String::new(),
                }
            };
            vec![
                p.name.clone(),
                param_type(&p.kind).to_string(),
                if p.required { "yes" } else { "no" }.to_string(),
                p.default.as_ref().map(cell).unwrap_or_default(),
                allowed,
                p.description.clone(),
            ]
        })
        .collect();
    out.push(console::table(&headers, &rows, MAX_CELL));
    out.join("\n")
}

pub(super) fn describe(err: &reqwest::Error, method: &str, url: &str) -> String {
    let reason = if err.is_timeout() {
        "it took too long to answer".to_string()
    } else {
        let mut text = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(inner) = source {
            text = format!("{text}: {inner}");
            source = inner.source();
        }
        text
    };
    format!("{method} {}: {reason}", util::go_quote(url))
}

pub(super) async fn exec_list(relay: &str, token: &str, deadline: Instant) -> i32 {
    let url = format!("{relay}/api/sessions/{token}/meta");
    let http = match client() {
        Ok(c) => c,
        Err(err) => {
            crate::errln!("{err}");
            return 1;
        }
    };
    let resp = match http.get(&url).timeout(remaining(deadline)).send().await {
        Ok(r) => r,
        Err(err) => {
            crate::errln!(
                "request failed: {}",
                plain_error(&describe(&err, "Get", &url))
            );
            return 1;
        }
    };

    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(err) => {
            crate::errln!("could not read session metadata: {err}");
            return 1;
        }
    };
    let meta: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            crate::errln!("could not read session metadata: {err}");
            return 1;
        }
    };
    if !meta.get("ready").and_then(Value::as_bool).unwrap_or(false) {
        crate::errln!("no host is connected to this session yet");
        return 1;
    }
    if let Some(kinds) = meta.get("allowed_kinds").and_then(Value::as_array) {
        for kind in kinds.iter().filter_map(Value::as_str) {
            crate::outln!("{kind}");
        }
    }
    0
}

use super::exec::Reply;
use super::exec_describe::describe;
use crate::capabilities::pwsh::normalize_numbers;
use crate::util;
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

pub(super) fn client() -> Result<reqwest::Client, String> {
    util::install_crypto_provider();
    reqwest::Client::builder()
        .build()
        .map_err(|e| format!("build the HTTP client: {e}"))
}

pub(super) fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

pub fn build_exec_payload(
    kind: &str,
    pairs: &[String],
    args_json: &str,
) -> Result<Map<String, Value>, String> {
    let mut payload = Map::new();
    payload.insert("kind".to_string(), json!(kind));

    if !args_json.trim().is_empty() {
        match serde_json::from_str::<Value>(args_json) {
            Ok(Value::Object(parsed)) => {
                for (k, v) in parsed {
                    payload.insert(k, normalize_numbers(v));
                }
            }
            Ok(Value::Null) => {}
            Ok(other) => {
                let kind_name = match other {
                    Value::Array(_) => "a list",
                    Value::String(_) => "text",
                    Value::Number(_) => "a number",
                    _ => "true or false",
                };
                return Err(format!(
                    "--args-json needs a JSON object such as {{\"count\": 4}}, but got {kind_name}"
                ));
            }
            Err(err) => return Err(format!("--args-json isn't valid JSON: {err}")),
        }
    }

    for pair in pairs {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) if !k.is_empty() => (k, v),
            _ => {
                return Err(format!(
                    "--arg must be key=value, got {}",
                    util::go_quote(pair)
                ));
            }
        };
        let typed = serde_json::from_str::<Value>(value)
            .map(normalize_numbers)
            .unwrap_or_else(|_| Value::String(value.to_string()));
        payload.insert(key.to_string(), typed);
    }
    Ok(payload)
}

pub fn view_token_from(input: &str) -> String {
    let s = input.trim();
    if let Some(i) = s.rfind("/v/") {
        return s[i + 3..].trim_matches('/').to_string();
    }
    s.trim_matches('/').to_string()
}

async fn post_json(
    url: &str,
    payload: &Map<String, Value>,
    deadline: Instant,
) -> Result<(Vec<u8>, u16), String> {
    let body = serde_json::to_vec(payload).map_err(|e| format!("encode the request body: {e}"))?;
    let resp = client()?
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .timeout(remaining(deadline))
        .send()
        .await
        .map_err(|e| describe(&e, "Post", url))?;
    let status = resp.status().as_u16();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read the relay response: {e}"))?;
    Ok((bytes.to_vec(), status))
}

pub(super) async fn run_command(
    relay: &str,
    token: &str,
    payload: &Map<String, Value>,
    timeout: Duration,
    deadline: Instant,
) -> Result<(Vec<u8>, u16), String> {
    let run_url = format!(
        "{relay}/api/sessions/{token}/run?wait_ms={}",
        timeout.as_millis()
    );
    let (body, status) = post_json(&run_url, payload, deadline).await?;
    if status != 404 {
        return Ok((body, status));
    }

    if let Ok(Value::Object(probe)) = serde_json::from_slice::<Value>(&body) {
        let msg = probe.get("error").and_then(Value::as_str).unwrap_or("");
        if msg.contains("view token") {
            return Ok((body, status));
        }
    }
    queue_and_await(relay, token, payload, timeout, deadline).await
}

async fn queue_and_await(
    relay: &str,
    token: &str,
    payload: &Map<String, Value>,
    timeout: Duration,
    ctx_deadline: Instant,
) -> Result<(Vec<u8>, u16), String> {
    let (ack_body, ack_status) = post_json(
        &format!("{relay}/api/sessions/{token}/cmd"),
        payload,
        ctx_deadline,
    )
    .await?;
    if ack_status != 200 {
        return Ok((ack_body, ack_status));
    }

    let command_id = match serde_json::from_slice::<Value>(&ack_body) {
        Ok(v) => v
            .get("command_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Err(_) => String::new(),
    };
    if command_id.is_empty() {
        return Ok((ack_body, ack_status));
    }

    let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
    let deadline = Instant::now() + timeout;
    let result_url = format!("{relay}/api/sessions/{token}/cmd/{command_id}");
    let http = client()?;

    while Instant::now() < deadline {
        let wait = remaining(deadline).min(Duration::from_secs(25));
        let url = format!("{result_url}?wait_ms={}", wait.as_millis());
        let resp = http
            .get(&url)
            .timeout(remaining(ctx_deadline))
            .send()
            .await
            .map_err(|e| describe(&e, "Get", &url))?;
        let status = resp.status().as_u16();
        let raw = resp
            .bytes()
            .await
            .map_err(|e| format!("read the result response: {e}"))?
            .to_vec();

        let got: Reply = match serde_json::from_slice::<Value>(&raw) {
            Ok(Value::Object(map)) => map,
            _ => Reply::new(),
        };

        match status {
            200 => {
                if let Some(Value::Object(inner)) = got.get("payload") {
                    return Ok((reshape_result(inner, kind, &command_id), 200));
                }
                if got.get("status") == Some(&json!("pending")) {
                    continue;
                }
                return Ok((raw, 200));
            }
            410 => return Ok((raw, 410)),
            404 => continue,
            other => return Ok((raw, other)),
        }
    }

    let pending = json!({
        "command_id": command_id,
        "kind": kind,
        "status": "pending",
        "ok": false,
        "result_url": format!("/api/sessions/{token}/cmd/{command_id}"),
    });
    Ok((serde_json::to_vec(&pending).unwrap_or_default(), 202))
}

pub fn reshape_result(inner: &Map<String, Value>, kind: &str, command_id: &str) -> Vec<u8> {
    let ok = inner.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let field = |key: &str| inner.get(key).cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("command_id".into(), json!(command_id));
    out.insert("kind".into(), json!(kind));
    out.insert("ok".into(), json!(ok));
    out.insert("status".into(), json!(if ok { "ok" } else { "error" }));
    out.insert("result".into(), field("result"));
    out.insert("error".into(), field("error"));
    out.insert("elapsed_ms".into(), field("elapsed_ms"));
    if let Some(detail) = inner.get("detail") {
        out.insert("detail".into(), detail.clone());
    }
    serde_json::to_vec(&Value::Object(out))
        .unwrap_or_else(|_| br#"{"ok":false,"error":"could not read the relay's result"}"#.to_vec())
}

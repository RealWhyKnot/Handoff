use super::relay::PING_INTERVAL;
use super::relay::PING_TIMEOUT;
use crate::dispatch::{Clamp, Outcome, Spec};
use crate::util;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Command {
    pub id: String,
    pub kind: String,
    pub timeout_ms: i64,
    pub extras: Map<String, Value>,
}

impl Command {
    pub fn parse(data: &str) -> Option<Command> {
        let Ok(Value::Object(mut raw)) = serde_json::from_str::<Value>(data) else {
            return None;
        };
        let mut cmd = Command::default();
        if let Some(Value::String(s)) = raw.remove("id") {
            cmd.id = s;
        }
        if let Some(Value::String(s)) = raw.remove("kind") {
            cmd.kind = s;
        }
        if let Some(v) = raw.remove("timeout_ms")
            && let Some(n) = v.as_i64()
        {
            cmd.timeout_ms = n;
        }
        cmd.extras = raw;
        Some(cmd)
    }
}

#[derive(Serialize)]
struct TelemetryEvent<'a> {
    ts: i64,
    kind: &'a str,
    payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecvError {
    Cancelled,
    Stale,
    BridgeClosed,
    Closed { code: u16, reason: String },
    Eof,
    Io(String),
}

impl RecvError {
    pub fn is_eof(&self) -> bool {
        matches!(self, RecvError::Eof | RecvError::Closed { .. })
    }
}

fn close_code_name(code: u16) -> String {
    let name = match code {
        1000 => "StatusNormalClosure",
        1001 => "StatusGoingAway",
        1002 => "StatusProtocolError",
        1003 => "StatusUnsupportedData",
        1005 => "StatusNoStatusRcvd",
        1006 => "StatusAbnormalClosure",
        1007 => "StatusInvalidFramePayloadData",
        1008 => "StatusPolicyViolation",
        1009 => "StatusMessageTooBig",
        1010 => "StatusMandatoryExtension",
        1011 => "StatusInternalError",
        1012 => "StatusServiceRestart",
        1013 => "StatusTryAgainLater",
        1014 => "StatusBadGateway",
        1015 => "StatusTLSHandshake",
        other => return format!("StatusCode({other})"),
    };
    name.to_string()
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecvError::Cancelled => f.write_str("stopped"),
            RecvError::Stale => f.write_str("bridge stale: ping timeout"),
            RecvError::BridgeClosed => f.write_str("bridge is closed"),
            RecvError::Closed { code, reason } => write!(
                f,
                "ws closed: code={} ({}) reason={:?}: EOF",
                code,
                close_code_name(*code),
                reason
            ),
            RecvError::Eof => f.write_str("EOF"),
            RecvError::Io(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for RecvError {}

#[derive(Debug, Clone, Copy)]
pub struct KeepaliveTiming {
    pub interval: Duration,
    pub timeout: Duration,
}

impl Default for KeepaliveTiming {
    fn default() -> Self {
        KeepaliveTiming {
            interval: PING_INTERVAL,
            timeout: PING_TIMEOUT,
        }
    }
}

pub(super) enum Incoming {
    Text(String),
    Closed(Option<(u16, String)>),
    Failed(String),
}

#[derive(Clone)]
pub(super) struct Hello {
    pub(super) hostname: String,
    pub(super) version: String,
    pub(super) capabilities: Vec<String>,
    pub(super) specs: Vec<Spec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    Queued,
}

pub(super) fn result_payload(id: &str, out: &Outcome) -> Result<Value, String> {
    let mut payload = Map::new();
    payload.insert("id".into(), json!(id));
    payload.insert("ok".into(), json!(out.ok));
    payload.insert("result".into(), out.result.clone());
    payload.insert("error".into(), json!(out.error));
    payload.insert("elapsed_ms".into(), json!(out.elapsed_ms));
    if let Some(detail) = &out.detail {
        payload.insert("detail".into(), detail.clone());
    }
    if !out.clamped.is_empty() {
        payload.insert(
            "clamped".into(),
            serde_json::to_value::<&Vec<Clamp>>(&out.clamped)
                .map_err(|e| format!("encode the clamp list: {e}"))?,
        );
    }
    if !out.ignored.is_empty() {
        payload.insert("ignored".into(), json!(out.ignored));
    }
    Ok(Value::Object(payload))
}

pub(super) fn frame_text(kind: &str, payload: Value) -> Result<String, String> {
    let event = TelemetryEvent {
        ts: util::now_ms(),
        kind,
        payload,
    };
    serde_json::to_string(&event).map_err(|e| format!("encode a relay frame: {e}"))
}

pub(super) fn salvage_id(text: &str) -> Option<String> {
    let at = text.find("\"id\"")?;
    let rest = text[at + 4..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let id = &rest[..end];
    if id.is_empty() || id.len() > 128 || id.contains('\\') {
        return None;
    }
    Some(id.to_string())
}

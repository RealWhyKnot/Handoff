use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::dispatch::{handler, CapResult, Failure, Handler, HandlerError};

use super::pwsh;

pub fn normalize(v: Value) -> Value {
    match v {
        Value::String(s) => match ms_json_date(&s) {
            Some(iso) => Value::String(iso),
            None => Value::String(s),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, normalize(v))).collect())
        }
        other => other,
    }
}

pub fn ms_json_date(s: &str) -> Option<String> {
    let inner = s.strip_prefix("/Date(")?.strip_suffix(")/")?;
    let body = inner.strip_prefix('-').unwrap_or(inner);
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 15 {
        return None;
    }
    let offset = &body[digits..];
    if !offset.is_empty() {
        let b = offset.as_bytes();
        if b.len() != 5 || !(b[0] == b'+' || b[0] == b'-') || !b[1..].iter().all(u8::is_ascii_digit)
        {
            return None;
        }
    }
    let ms: i64 = inner[..inner.len() - offset.len()].parse().ok()?;
    Some(crate::util::format_utc_ms(ms))
}

pub fn parse_json(out: &[u8]) -> Result<Value, Failure> {
    pwsh::parse_pwsh_json(out).map(normalize)
}

pub async fn run_pwsh_json(cancel: &CancellationToken, script: &str) -> Result<Value, Failure> {
    pwsh::run_pwsh_json(cancel, script).await.map(normalize)
}

pub fn json_script_handler(script: &'static str) -> Handler {
    handler(move |ctx, _args| async move {
        let result: CapResult = run_pwsh_json(&ctx.cancel, script)
            .await
            .map_err(HandlerError::from);
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ms_dates_become_iso_utc() {
        assert_eq!(
            ms_json_date("/Date(1792200615341)/").unwrap(),
            "2026-10-17T01:30:15.341Z"
        );
        assert_eq!(
            ms_json_date("/Date(0)/").unwrap(),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            ms_json_date("/Date(-86400000)/").unwrap(),
            "1969-12-31T00:00:00.000Z"
        );
        assert_eq!(
            ms_json_date("/Date(1150848000000+0100)/").unwrap(),
            "2006-06-21T00:00:00.000Z"
        );
    }

    #[test]
    fn near_misses_are_left_alone() {
        for s in [
            "/Date()/",
            "/Date(abc)/",
            "/Date(12)/ ",
            "Date(12)",
            "/Date(12+1)/",
            "/Date(+12)/",
            "/Date(12-01000)/",
            "/Date(99999999999999999999)/",
        ] {
            assert_eq!(ms_json_date(s), None, "{s}");
        }
    }

    #[test]
    fn normalize_walks_nested_values() {
        let v = parse_json(br#"[{"CreationDate":"\/Date(0)\/","Name":"x","n":3,"inner":{"d":["\/Date(1000)\/"]}}]"#)
            .unwrap();
        assert_eq!(
            v,
            json!([{"CreationDate": "1970-01-01T00:00:00.000Z", "Name": "x", "n": 3,
                    "inner": {"d": ["1970-01-01T00:00:01.000Z"]}}])
        );
    }

    #[tokio::test]
    async fn windows_powershell_dates_are_normalized_end_to_end() {
        if cfg!(windows) {
            let v = run_pwsh_json(
                &CancellationToken::new(),
                "[ordered]@{when = [datetime]::new(2020,1,2,3,4,5,[DateTimeKind]::Utc)} | ConvertTo-Json -Compress",
            )
            .await
            .unwrap();
            assert_eq!(v["when"], json!("2020-01-02T03:04:05.000Z"));
        }
    }
}

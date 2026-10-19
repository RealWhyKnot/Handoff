use std::process::Stdio;

use serde_json::Value;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::dispatch::{handler, CapResult, Failure, Handler};

#[derive(Debug, Clone, Default)]
pub struct PwshResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i64,
}

#[allow(clippy::result_large_err)]
pub async fn run_pwsh_full(
    cancel: &CancellationToken,
    script: &str,
) -> Result<PwshResult, (PwshResult, Failure)> {
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Err(failure_for(
                PwshResult {
                    exit_code: -1,
                    ..PwshResult::default()
                },
                e.to_string(),
            ));
        }
    };

    let output = tokio::select! {
        out = child.wait_with_output() => out,
        _ = cancel.cancelled() => {
            return Err(failure_for(
                PwshResult { exit_code: -1, ..PwshResult::default() },
                "signal: killed".to_string(),
            ));
        }
    };

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            return Err(failure_for(
                PwshResult {
                    exit_code: -1,
                    ..PwshResult::default()
                },
                e.to_string(),
            ));
        }
    };

    let res = PwshResult {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code().map(i64::from).unwrap_or(-1),
    };
    if output.status.success() {
        return Ok(res);
    }
    let fallback = format!("exit status {}", res.exit_code);
    Err(failure_for(res, fallback))
}

fn failure_for(res: PwshResult, fallback: String) -> (PwshResult, Failure) {
    let stderr_text = String::from_utf8_lossy(&res.stderr).into_owned();
    let msg = stderr_text.trim();
    let msg = if msg.is_empty() {
        fallback.as_str()
    } else {
        msg
    };
    let failure = Failure {
        message: format!("powershell exited {}: {}", res.exit_code, msg),
        exit_code: Some(res.exit_code),
        stdout: String::from_utf8_lossy(&res.stdout).into_owned(),
        stderr: stderr_text,
    };
    (res, failure)
}

pub async fn run_pwsh(cancel: &CancellationToken, script: &str) -> Result<Vec<u8>, Failure> {
    run_pwsh_full(cancel, script)
        .await
        .map(|r| r.stdout)
        .map_err(|(_, f)| f)
}

pub async fn run_pwsh_json(cancel: &CancellationToken, script: &str) -> Result<Value, Failure> {
    let out = run_pwsh(cancel, script).await?;
    parse_pwsh_json(&out)
}

pub fn parse_pwsh_json(out: &[u8]) -> Result<Value, Failure> {
    let text = String::from_utf8_lossy(out);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => Ok(normalize_numbers(v)),
        Err(err) => {
            if let Some(i) = trimmed.find(['[', '{']) {
                if i > 0 {
                    if let Ok(v) = serde_json::from_str::<Value>(&trimmed[i..]) {
                        return Ok(normalize_numbers(v));
                    }
                }
            }
            Err(Failure::new(format!(
                "decode powershell json: {} (got {} bytes)",
                err,
                trimmed.len()
            )))
        }
    }
}

pub fn normalize_numbers(v: Value) -> Value {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if n.is_f64() && f.fract() == 0.0 && f.abs() < 9.0e15 {
                    return Value::from(f as i64);
                }
            }
            Value::Number(n)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_numbers).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, normalize_numbers(v)))
                .collect(),
        ),
        other => other,
    }
}

pub fn ps_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn json_script_handler(script: &'static str) -> Handler {
    handler(move |ctx, _args| async move {
        let result: CapResult = run_pwsh_json(&ctx.cancel, script)
            .await
            .map_err(crate::dispatch::HandlerError::from);
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn single_quotes_are_doubled() {
        assert_eq!(ps_single_quote("it's"), "'it''s'");
        assert_eq!(ps_single_quote(""), "''");
    }

    #[test]
    fn empty_output_is_an_empty_list() {
        assert_eq!(parse_pwsh_json(b"  \r\n").unwrap(), json!([]));
    }

    #[test]
    fn json_is_parsed() {
        assert_eq!(
            parse_pwsh_json(br#"{"a":[1,2,{"b":null}]}"#).unwrap(),
            json!({"a": [1, 2, {"b": null}]})
        );
    }

    #[test]
    fn leading_warnings_are_skipped() {
        let out = b"WARNING: something noisy\r\n{\"ok\":true}\r\n";
        assert_eq!(parse_pwsh_json(out).unwrap(), json!({"ok": true}));
    }

    #[test]
    fn garbage_is_an_error_with_a_byte_count() {
        let err = parse_pwsh_json(b"not json at all").unwrap_err();
        assert!(
            err.message.contains("decode powershell json"),
            "{}",
            err.message
        );
        assert!(err.message.contains("15 bytes"), "{}", err.message);
    }

    #[test]
    fn invalid_utf8_does_not_break_decoding() {
        let mut out = br#"{"name":"caf"#.to_vec();
        out.push(0x82);
        out.extend_from_slice(br#""}"#);
        let v = parse_pwsh_json(&out).unwrap();
        assert!(v["name"].as_str().unwrap().starts_with("caf"));
    }

    #[test]
    fn whole_floats_are_written_as_integers() {
        let v = parse_pwsh_json(br#"{"SizeGB":238.0,"Ratio":0.5,"n":3}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"Ratio":0.5,"SizeGB":238,"n":3}"#
        );
    }

    #[tokio::test]
    async fn missing_or_failing_shell_reports_a_failure() {
        if cfg!(windows) {
            let cancel = CancellationToken::new();
            let err = run_pwsh(&cancel, "exit 3").await.unwrap_err();
            assert_eq!(err.exit_code, Some(3));
            assert!(
                err.message.starts_with("powershell exited 3"),
                "{}",
                err.message
            );
            let ok = run_pwsh(&cancel, "Write-Output 'hi'").await.unwrap();
            assert_eq!(String::from_utf8_lossy(&ok).trim(), "hi");
        }
    }

    #[tokio::test]
    async fn cancellation_stops_the_script() {
        if cfg!(windows) {
            let cancel = CancellationToken::new();
            let c = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                c.cancel();
            });
            let started = std::time::Instant::now();
            let err = run_pwsh(&cancel, "Start-Sleep 30").await.unwrap_err();
            assert!(started.elapsed().as_secs() < 20);
            assert!(err.message.contains("powershell exited"), "{}", err.message);
        }
    }
}

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio_util::sync::CancellationToken;

use crate::dispatch::{handler, CapResult, Failure, Handler, Progress};

pub const STDOUT_CAP: usize = 4 << 20;
pub const STDERR_CAP: usize = 512 << 10;
pub const MAX_PROCESSES: usize = 4;
const LINE_CAP: usize = 16 << 10;
const MESSAGE_CAP: usize = 4 << 10;
const BOOTSTRAP: &str = ". ([scriptblock]::Create((New-Object IO.StreamReader([Console]::OpenStandardInput(),(New-Object Text.UTF8Encoding $false))).ReadToEnd()))";
const STATUS_TAIL: &str = "\n\nif (-not $?) { exit 1 }\n";

static PROCESSES: Semaphore = Semaphore::const_new(MAX_PROCESSES);

#[derive(Debug, Clone, Default)]
pub struct PwshResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Echo {
    Stderr,
    Both,
}

pub async fn process_slot(cancel: &CancellationToken) -> Result<SemaphorePermit<'static>, Failure> {
    tokio::select! {
        permit = PROCESSES.acquire() => {
            permit.map_err(|_| Failure::new("could not start a process: the process limit is closed"))
        }
        _ = cancel.cancelled() => Err(Failure::new("cancelled while waiting for a free process slot")),
    }
}

#[allow(clippy::result_large_err)]
pub async fn run_pwsh_full(
    cancel: &CancellationToken,
    script: &str,
) -> Result<PwshResult, (PwshResult, Failure)> {
    run_pwsh_echo(cancel, script, None, Echo::Both).await
}

#[allow(clippy::result_large_err)]
pub async fn run_pwsh_echo(
    cancel: &CancellationToken,
    script: &str,
    progress: Option<Progress>,
    echo: Echo,
) -> Result<PwshResult, (PwshResult, Failure)> {
    let spawn_failed = |e: std::io::Error| {
        failure_for(
            PwshResult {
                exit_code: -1,
                ..PwshResult::default()
            },
            e.to_string(),
        )
    };
    let _slot = process_slot(cancel).await.map_err(|f| {
        (
            PwshResult {
                exit_code: -1,
                ..PwshResult::default()
            },
            f,
        )
    })?;
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            BOOTSTRAP,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(spawn_failed)?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(spawn_failed(std::io::Error::other("pipes not captured")));
    };
    let mut body = Vec::with_capacity(script.len() + STATUS_TAIL.len());
    body.extend_from_slice(script.as_bytes());
    body.extend_from_slice(STATUS_TAIL.as_bytes());
    let feed = async move {
        let wrote = stdin.write_all(&body).await;
        drop(stdin);
        wrote
    };
    let out_echo = progress.clone().filter(|_| echo == Echo::Both);
    let run = async {
        tokio::join!(
            feed,
            collect_lines(stdout, out_echo, STDOUT_CAP),
            collect_lines(stderr, progress, STDERR_CAP),
            child.wait()
        )
    };
    let (fed, out, err, status) = tokio::select! {
        r = run => r,
        _ = cancel.cancelled() => {
            return Err(failure_for(
                PwshResult { exit_code: -1, ..PwshResult::default() },
                "cancelled".to_string(),
            ));
        }
    };
    let status = status.map_err(spawn_failed)?;
    let (stdout, stdout_truncated) = out.map_err(spawn_failed)?;
    let (stderr, stderr_truncated) = err.map_err(spawn_failed)?;
    let res = PwshResult {
        stdout,
        stderr,
        exit_code: status.code().map(i64::from).unwrap_or(-1),
        stdout_truncated,
        stderr_truncated,
    };
    if let Err(e) = fed {
        if res.stderr.is_empty() {
            let msg = format!("could not send the script to powershell: {e}");
            return Err(failure_for(res, msg));
        }
    }
    if status.success() {
        return Ok(res);
    }
    Err(failure_for(res, "no error output".to_string()))
}

async fn collect_lines<R: AsyncRead + Unpin>(
    pipe: R,
    progress: Option<Progress>,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut reader = BufReader::new(pipe);
    let mut all = Vec::new();
    let mut line = Vec::new();
    let mut truncated = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            if let (Some(progress), false) = (&progress, line.is_empty()) {
                progress(String::from_utf8_lossy(&line).into_owned());
            }
            return Ok((all, truncated));
        }
        let (used, eol) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        let chunk = &buf[..used];
        let kept = if truncated {
            0
        } else {
            let room = cap - all.len();
            if chunk.len() > room {
                truncated = true;
                room
            } else {
                chunk.len()
            }
        };
        all.extend_from_slice(&chunk[..kept]);
        let line_room = LINE_CAP.saturating_sub(line.len()).min(kept);
        line.extend_from_slice(&chunk[..line_room]);
        reader.consume(used);
        if eol || line.len() >= LINE_CAP || (truncated && !line.is_empty()) {
            if let (Some(progress), false) = (&progress, line.is_empty()) {
                progress(String::from_utf8_lossy(&line).into_owned());
            }
            line.clear();
        }
    }
}

fn failure_for(res: PwshResult, fallback: String) -> (PwshResult, Failure) {
    let stderr_text = String::from_utf8_lossy(&res.stderr).into_owned();
    let msg = stderr_text.trim();
    let msg = if msg.is_empty() {
        fallback.as_str()
    } else {
        msg
    };
    let msg = match msg.char_indices().nth(MESSAGE_CAP) {
        Some((i, _)) => format!("{}...", &msg[..i]),
        None => msg.to_string(),
    };
    let failure = Failure {
        message: format!("powershell exited {}: {}", res.exit_code, msg),
        exit_code: Some(res.exit_code),
        stdout: String::from_utf8_lossy(&res.stdout).into_owned(),
        stderr: stderr_text,
    };
    (res, failure)
}

pub fn truncation_note(res: &PwshResult) -> Option<String> {
    let out = format!("output was cut off at {} MiB", STDOUT_CAP >> 20);
    let err = format!("error output was cut off at {} KiB", STDERR_CAP >> 10);
    match (res.stdout_truncated, res.stderr_truncated) {
        (false, false) => None,
        (true, false) => Some(out),
        (false, true) => Some(err),
        (true, true) => Some(format!("{out}, and {err}")),
    }
}

pub async fn run_pwsh(cancel: &CancellationToken, script: &str) -> Result<Vec<u8>, Failure> {
    let res = run_pwsh_full(cancel, script).await.map_err(|(_, f)| f)?;
    if res.stdout_truncated {
        return Err(Failure {
            message: format!(
                "powershell printed more than {} MiB, so its output was cut off",
                STDOUT_CAP >> 20
            ),
            exit_code: Some(res.exit_code),
            ..Failure::default()
        });
    }
    Ok(res.stdout)
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

pub fn is_ps_single_quote(c: char) -> bool {
    matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}')
}

pub fn ps_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        out.push(c);
        if is_ps_single_quote(c) {
            out.push(c);
        }
    }
    out.push('\'');
    out
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
        assert_eq!(ps_single_quote("a\u{2018}b"), "'a\u{2018}\u{2018}b'");
        assert_eq!(ps_single_quote("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
        assert_eq!(ps_single_quote("a\u{201A}b"), "'a\u{201A}\u{201A}b'");
        assert_eq!(ps_single_quote("a\u{201B}b"), "'a\u{201B}\u{201B}b'");
        assert_eq!(ps_single_quote("\u{201C}x\u{201D}"), "'\u{201C}x\u{201D}'");
    }

    fn char_codes(s: &str) -> String {
        s.chars()
            .map(|c| (c as u32).to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    #[tokio::test]
    async fn curly_quotes_cannot_break_out_of_a_quoted_argument() {
        if !cfg!(windows) {
            return;
        }
        let cancel = CancellationToken::new();
        for q in ['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
            let hostile = format!("x{q}; Write-Output pwned; {q}y");
            let script = format!(
                "$v = {}\n($v.ToCharArray() | ForEach-Object {{ [int]$_ }}) -join ','",
                ps_single_quote(&hostile)
            );
            let out = run_pwsh(&cancel, &script).await.unwrap();
            let out = String::from_utf8_lossy(&out);
            assert_eq!(out.trim(), char_codes(&hostile), "quote U+{:04X}", q as u32);
            assert!(!out.contains("pwned"), "quote U+{:04X}", q as u32);
        }
    }

    #[tokio::test]
    async fn long_multiline_scripts_go_through_stdin() {
        if !cfg!(windows) {
            return;
        }
        let filler = "a".repeat(40 * 1024);
        let script = format!(
            "function Measure-It {{\n  param($s)\n  $s.Length\n}}\n$text = @'\n{filler}\n'@\n$x = 'caf\u{e9}'\nMeasure-It $text\n[int]$x[3]\n"
        );
        assert!(script.len() > 40 * 1024);
        let out = run_pwsh(&CancellationToken::new(), &script).await.unwrap();
        let lines: Vec<String> = String::from_utf8_lossy(&out)
            .lines()
            .map(|l| l.trim().to_string())
            .collect();
        assert_eq!(lines, vec![(40 * 1024).to_string(), "233".to_string()]);
    }

    #[tokio::test]
    async fn exit_status_follows_the_last_statement() {
        if !cfg!(windows) {
            return;
        }
        let cancel = CancellationToken::new();
        let err = run_pwsh(&cancel, "Get-Item 'C:\\handoff-missing-zz'")
            .await
            .unwrap_err();
        assert_eq!(err.exit_code, Some(1));
        assert!(
            err.message.contains("handoff-missing-zz"),
            "{}",
            err.message
        );
        let ok = run_pwsh(
            &cancel,
            "Write-Output a; Get-Item 'C:\\handoff-missing-zz' -ErrorAction SilentlyContinue; Write-Output b",
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&ok).trim(), "a\r\nb");
        let err = run_pwsh(&cancel, "throw 'boom'").await.unwrap_err();
        assert_eq!(err.exit_code, Some(1));
        assert!(err.message.contains("boom"), "{}", err.message);
    }

    #[tokio::test]
    async fn huge_output_is_cut_off_and_flagged() {
        if !cfg!(windows) {
            return;
        }
        let cancel = CancellationToken::new();
        let script = "$chunk = 'x' * 1MB\n1..5 | ForEach-Object { [Console]::Out.Write($chunk) }";
        let res = run_pwsh_full(&cancel, script).await.unwrap();
        assert_eq!(res.stdout.len(), STDOUT_CAP);
        assert!(res.stdout_truncated);
        assert!(!res.stderr_truncated);
        assert_eq!(
            truncation_note(&res).unwrap(),
            "output was cut off at 4 MiB"
        );
        let err = run_pwsh(&cancel, script).await.unwrap_err();
        assert_eq!(
            err.message,
            "powershell printed more than 4 MiB, so its output was cut off"
        );

        let small = run_pwsh_full(&cancel, "'hi'").await.unwrap();
        assert!(!small.stdout_truncated);
        assert!(truncation_note(&small).is_none());
    }

    #[tokio::test]
    async fn no_more_than_four_powershells_run_at_once() {
        if !cfg!(windows) {
            return;
        }
        let never = CancellationToken::new();
        let mut held = Vec::new();
        for _ in 0..MAX_PROCESSES {
            held.push(process_slot(&never).await.unwrap());
        }
        let waiting =
            tokio::spawn(async { run_pwsh(&CancellationToken::new(), "Write-Output ran").await });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(!waiting.is_finished());

        let gave_up = CancellationToken::new();
        let g = gave_up.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            g.cancel();
        });
        let err = run_pwsh(&gave_up, "Write-Output never").await.unwrap_err();
        assert_eq!(
            err.message,
            "cancelled while waiting for a free process slot"
        );

        drop(held);
        let out = waiting.await.unwrap().unwrap();
        assert_eq!(String::from_utf8_lossy(&out).trim(), "ran");
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

    fn recorder() -> (Progress, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let progress: Progress = std::sync::Arc::new(move |t| sink.lock().unwrap().push(t));
        (progress, seen)
    }

    fn trimmed(seen: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
        let mut lines: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|l| l.trim().to_string())
            .collect();
        lines.sort();
        lines
    }

    #[tokio::test]
    async fn echo_reports_each_line_and_keeps_the_full_output() {
        let script = "'one'; [Console]::Error.WriteLine('two'); 'three'";
        let (progress, seen) = recorder();
        let res = run_pwsh_echo(
            &CancellationToken::new(),
            script,
            Some(progress),
            Echo::Stderr,
        )
        .await
        .unwrap();
        assert_eq!(trimmed(&seen), vec!["two"]);
        assert_eq!(String::from_utf8_lossy(&res.stdout).trim(), "one\r\nthree");

        let (progress, seen) = recorder();
        run_pwsh_echo(
            &CancellationToken::new(),
            script,
            Some(progress),
            Echo::Both,
        )
        .await
        .unwrap();
        assert_eq!(trimmed(&seen), vec!["one", "three", "two"]);
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

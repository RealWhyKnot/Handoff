use std::process::Stdio;

use serde_json::{json, Value};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router, Spec};
use crate::picotool;

use super::consent::require_risk_consent;

const FAMILIES: [&str; 6] = [
    "rp2040",
    "rp2350-arm-s",
    "rp2350-arm-ns",
    "rp2350-riscv",
    "absolute",
    "data",
];

const CPUS: [&str; 2] = ["arm", "riscv"];

pub fn register(r: &mut Router) {
    let specs: Vec<Spec> = serde_json::from_str(include_str!("specs/pico.json"))
        .expect("embedded capability specs are valid JSON");
    for spec in specs {
        let h = match spec.kind.as_str() {
            "pico.list" => handler(pico_list),
            "pico.info" => handler(pico_info),
            "pico.bootsel" => handler(pico_bootsel),
            "pico.flash" => handler(pico_flash),
            "pico.verify" => handler(pico_verify),
            "pico.save" => handler(pico_save),
            "pico.reset" => handler(pico_reset),
            other => panic!("no handler for {other}"),
        };
        r.register_spec(spec, h);
    }
}

struct Run {
    stdout: String,
    stderr: String,
    error: Option<String>,
}

impl Run {
    fn failed(error: String) -> Run {
        Run {
            stdout: String::new(),
            stderr: String::new(),
            error: Some(error),
        }
    }
}

fn exit_text(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit status {}", code as u32),
        None => "signal: killed".to_string(),
    }
}

async fn run_picotool(cancel: &CancellationToken, args: &[String]) -> Run {
    let pt = match picotool::path() {
        Ok(p) => p,
        Err(e) => return Run::failed(e),
    };
    let _slot = match crate::capabilities::pwsh::process_slot(cancel).await {
        Ok(s) => s,
        Err(f) => return Run::failed(f.message),
    };
    let child = Command::new(&pt)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => return Run::failed(e.to_string()),
    };
    let output = tokio::select! {
        out = child.wait_with_output() => out,
        _ = cancel.cancelled() => return Run::failed("signal: killed".to_string()),
    };
    match output {
        Ok(o) => Run {
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            error: if o.status.success() {
                None
            } else {
                Some(exit_text(&o.status))
            },
        },
        Err(e) => Run::failed(e.to_string()),
    }
}

fn pico_string_arg(args: &Args, key: &str) -> String {
    match args.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

fn with_serial(mut args: Vec<String>, serial: &str) -> Vec<String> {
    if !serial.is_empty() {
        args.push("--id".to_string());
        args.push(serial.to_string());
    }
    args
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn reboot_options(
    mut cmd: Vec<String>,
    args: &Args,
    kind: &str,
) -> Result<Vec<String>, HandlerError> {
    let cpu = pico_string_arg(args, "cpu");
    if !cpu.is_empty() {
        if !CPUS.contains(&cpu.as_str()) {
            return Err(HandlerError::plain(format!(
                "{kind}: cpu must be 'arm' or 'riscv'"
            )));
        }
        cmd.push("-c".to_string());
        cmd.push(cpu);
    }
    let part = pico_string_arg(args, "partition");
    if !part.is_empty() {
        cmd.push("-g".to_string());
        cmd.push(part);
    }
    Ok(with_serial(cmd, &pico_string_arg(args, "serial")))
}

fn pico_bootsel_args(args: &Args) -> Result<Vec<String>, HandlerError> {
    reboot_options(strings(&["reboot", "-f", "-u"]), args, "pico.bootsel")
}

fn pico_reset_args(args: &Args) -> Result<Vec<String>, HandlerError> {
    reboot_options(strings(&["reboot"]), args, "pico.reset")
}

fn family_option(cmd: &mut Vec<String>, args: &Args, kind: &str) -> Result<(), HandlerError> {
    let family = pico_string_arg(args, "family");
    if !family.is_empty() {
        if !FAMILIES.contains(&family.as_str()) {
            return Err(HandlerError::plain(format!(
                "{kind}: unknown family {}",
                go_quote(&family)
            )));
        }
        cmd.push("--family".to_string());
        cmd.push(family);
    }
    Ok(())
}

fn go_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn pico_flash_args(args: &Args) -> Result<(Vec<String>, String), HandlerError> {
    let uf2 = pico_string_arg(args, "uf2_path");
    if uf2.is_empty() {
        return Err(HandlerError::plain("pico.flash: 'uf2_path' is required"));
    }
    let mut cmd = strings(&["load", "-fx"]);
    family_option(&mut cmd, args, "pico.flash")?;
    cmd.push(uf2.clone());
    Ok((with_serial(cmd, &pico_string_arg(args, "serial")), uf2))
}

fn pico_verify_args(args: &Args) -> Result<(Vec<String>, String), HandlerError> {
    let path = pico_string_arg(args, "file_path");
    if path.is_empty() {
        return Err(HandlerError::plain("pico.verify: 'file_path' is required"));
    }
    let mut cmd = strings(&["verify", "-f"]);
    family_option(&mut cmd, args, "pico.verify")?;
    cmd.push(path.clone());
    Ok((with_serial(cmd, &pico_string_arg(args, "serial")), path))
}

fn failed(run: &Run, reason: String) -> Value {
    json!({
        "ok": false,
        "reason": reason,
        "stderr": run.stderr.trim(),
        "stdout": run.stdout.trim(),
    })
}

async fn pico_list(ctx: Ctx, _args: Args) -> CapResult {
    let run = run_picotool(&ctx.cancel, &strings(&["info", "-a"])).await;
    if let Some(reason) = run.error.clone() {
        return Ok(failed(&run, reason));
    }
    Ok(json!({"ok": true, "raw": run.stdout, "format": "picotool-info-text"}))
}

async fn pico_info(ctx: Ctx, args: Args) -> CapResult {
    let cmd = with_serial(
        strings(&["info", "-a", "-m", "-d", "-l"]),
        &pico_string_arg(&args, "serial"),
    );
    let run = run_picotool(&ctx.cancel, &cmd).await;
    if let Some(reason) = run.error {
        return Ok(json!({"ok": false, "reason": reason, "stderr": run.stderr.trim()}));
    }
    Ok(json!({"ok": true, "raw": run.stdout}))
}

async fn pico_bootsel(ctx: Ctx, args: Args) -> CapResult {
    let cmd = pico_bootsel_args(&args)?;
    require_risk_consent(
        &ctx.cancel,
        "pico.bootsel",
        "Reboots an attached Pico into BOOTSEL mode. Connected software may lose its current device connection.",
    )
    .await?;
    let run = run_picotool(&ctx.cancel, &cmd).await;
    if let Some(reason) = run.error.clone() {
        return Ok(failed(&run, reason));
    }
    Ok(json!({"ok": true, "raw": run.stdout}))
}

async fn pico_flash(ctx: Ctx, args: Args) -> CapResult {
    let (cmd, uf2) = pico_flash_args(&args)?;
    require_risk_consent(
        &ctx.cancel,
        "pico.flash",
        "Flashes firmware to an attached Pico from a UF2 file. Bad firmware can make the device stop working until it is reflashed.",
    )
    .await?;
    let run = run_picotool(&ctx.cancel, &cmd).await;
    if let Some(reason) = run.error.clone() {
        return Ok(failed(&run, reason));
    }
    Ok(json!({"ok": true, "uf2": uf2, "raw": run.stdout}))
}

async fn pico_verify(ctx: Ctx, args: Args) -> CapResult {
    let (cmd, path) = pico_verify_args(&args)?;
    let run = run_picotool(&ctx.cancel, &cmd).await;
    if let Some(reason) = run.error.clone() {
        return Ok(json!({
            "ok": false,
            "reason": reason,
            "stderr": run.stderr.trim(),
            "stdout": run.stdout.trim(),
            "file_path": path,
        }));
    }
    Ok(json!({"ok": true, "file_path": path, "raw": run.stdout}))
}

async fn pico_save(ctx: Ctx, args: Args) -> CapResult {
    let out = pico_string_arg(&args, "out_path");
    if out.is_empty() {
        return Err(HandlerError::plain("pico.save: 'out_path' is required"));
    }
    require_risk_consent(
        &ctx.cancel,
        "pico.save",
        "Writes a Pico flash dump to a file on this computer. Existing files may be replaced by picotool behavior.",
    )
    .await?;
    let cmd = with_serial(
        vec!["save".to_string(), "-a".to_string(), out.clone()],
        &pico_string_arg(&args, "serial"),
    );
    let run = run_picotool(&ctx.cancel, &cmd).await;
    if let Some(reason) = run.error.clone() {
        return Ok(failed(&run, reason));
    }
    Ok(json!({"ok": true, "out_path": out, "raw": run.stdout}))
}

async fn pico_reset(ctx: Ctx, args: Args) -> CapResult {
    let cmd = pico_reset_args(&args)?;
    require_risk_consent(
        &ctx.cancel,
        "pico.reset",
        "Reboots an attached Pico. Connected software may lose its current device connection.",
    )
    .await?;
    let run = run_picotool(&ctx.cancel, &cmd).await;
    if let Some(reason) = run.error.clone() {
        return Ok(failed(&run, reason));
    }
    Ok(json!({"ok": true, "raw": run.stdout}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn v(parts: &[&str]) -> Vec<String> {
        strings(parts)
    }

    #[test]
    fn with_serial_only_appends_when_non_empty() {
        assert_eq!(with_serial(v(&["info", "-a"]), ""), v(&["info", "-a"]));
        assert_eq!(
            with_serial(v(&["info", "-a"]), "E660583883278628"),
            v(&["info", "-a", "--id", "E660583883278628"])
        );
    }

    #[test]
    fn string_arg_trims_and_defaults() {
        let a = args(json!({"present": "  abc  ", "empty": "", "only_ws": "   ", "non_str": 42}));
        assert_eq!(pico_string_arg(&a, "present"), "abc");
        assert_eq!(pico_string_arg(&a, "missing"), "");
        assert_eq!(pico_string_arg(&a, "empty"), "");
        assert_eq!(pico_string_arg(&a, "only_ws"), "");
        assert_eq!(pico_string_arg(&a, "non_str"), "");
    }

    #[test]
    fn bootsel_args_defaults() {
        assert_eq!(
            pico_bootsel_args(&args(json!({}))).unwrap(),
            v(&["reboot", "-f", "-u"])
        );
    }

    #[test]
    fn bootsel_args_with_cpu_partition_and_serial() {
        let got = pico_bootsel_args(&args(
            json!({"cpu": "riscv", "partition": "boot-a", "serial": "ABCD1234"}),
        ))
        .unwrap();
        assert_eq!(
            got,
            v(&["reboot", "-f", "-u", "-c", "riscv", "-g", "boot-a", "--id", "ABCD1234"])
        );
    }

    #[test]
    fn bootsel_args_rejects_unknown_cpu() {
        let err = pico_bootsel_args(&args(json!({"cpu": "x86"}))).unwrap_err();
        assert_eq!(err.message, "pico.bootsel: cpu must be 'arm' or 'riscv'");
    }

    #[test]
    fn reset_args_defaults() {
        assert_eq!(pico_reset_args(&args(json!({}))).unwrap(), v(&["reboot"]));
    }

    #[test]
    fn reset_args_append_cpu_partition_and_serial() {
        let got = pico_reset_args(&args(
            json!({"cpu": "arm", "partition": "0", "serial": "DEADBEEF"}),
        ))
        .unwrap();
        assert_eq!(
            got,
            v(&["reboot", "-c", "arm", "-g", "0", "--id", "DEADBEEF"])
        );
    }

    #[test]
    fn reset_args_reject_unknown_cpu() {
        let err = pico_reset_args(&args(json!({"cpu": "mips"}))).unwrap_err();
        assert_eq!(err.message, "pico.reset: cpu must be 'arm' or 'riscv'");
    }

    #[test]
    fn flash_args_require_uf2() {
        let err = pico_flash_args(&args(json!({}))).unwrap_err();
        assert!(err.message.contains("uf2_path"), "{}", err.message);
    }

    #[test]
    fn flash_args_defaults() {
        let (got, uf2) = pico_flash_args(&args(json!({"uf2_path": "C:\\fw\\app.uf2"}))).unwrap();
        assert_eq!(uf2, "C:\\fw\\app.uf2");
        assert_eq!(got, v(&["load", "-fx", "C:\\fw\\app.uf2"]));
    }

    #[test]
    fn flash_args_append_family_and_serial() {
        let (got, _) = pico_flash_args(&args(json!({
            "uf2_path": "C:\\fw\\app.uf2",
            "family": "rp2350-arm-s",
            "serial": "E0C912D24340",
        })))
        .unwrap();
        assert_eq!(
            got,
            v(&[
                "load",
                "-fx",
                "--family",
                "rp2350-arm-s",
                "C:\\fw\\app.uf2",
                "--id",
                "E0C912D24340"
            ])
        );
    }

    #[test]
    fn flash_args_reject_unknown_family() {
        let err = pico_flash_args(&args(
            json!({"uf2_path": "C:\\fw\\app.uf2", "family": "rp9999"}),
        ))
        .unwrap_err();
        assert_eq!(err.message, "pico.flash: unknown family \"rp9999\"");
    }

    #[test]
    fn verify_args_require_file_path() {
        let err = pico_verify_args(&args(json!({}))).unwrap_err();
        assert!(err.message.contains("file_path"), "{}", err.message);
    }

    #[test]
    fn verify_args_defaults() {
        let (got, path) = pico_verify_args(&args(json!({"file_path": "C:\\fw\\app.uf2"}))).unwrap();
        assert_eq!(path, "C:\\fw\\app.uf2");
        assert_eq!(got, v(&["verify", "-f", "C:\\fw\\app.uf2"]));
    }

    #[test]
    fn verify_args_append_family_and_serial() {
        let (got, _) = pico_verify_args(&args(json!({
            "file_path": "C:\\fw\\app.uf2",
            "family": "rp2040",
            "serial": "EEEEEEEEEEEEEEEE",
        })))
        .unwrap();
        assert_eq!(
            got,
            v(&[
                "verify",
                "-f",
                "--family",
                "rp2040",
                "C:\\fw\\app.uf2",
                "--id",
                "EEEEEEEEEEEEEEEE"
            ])
        );
    }

    #[test]
    fn verify_args_reject_unknown_family() {
        let err = pico_verify_args(&args(
            json!({"file_path": "C:\\fw\\app.uf2", "family": "abcd"}),
        ))
        .unwrap_err();
        assert!(err.message.contains("unknown family"), "{}", err.message);
    }

    #[test]
    fn go_quote_escapes_like_go() {
        assert_eq!(go_quote("rp9999"), "\"rp9999\"");
        assert_eq!(go_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn registers_all_kinds() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(
            r.kinds(),
            vec![
                "pico.bootsel",
                "pico.flash",
                "pico.info",
                "pico.list",
                "pico.reset",
                "pico.save",
                "pico.verify",
            ]
        );
    }
}

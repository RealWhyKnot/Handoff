use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::proc::{fill, int_arg, string_arg};
use super::pwsh::ps_single_quote;
use super::pwsh_out::{json_script_handler, run_pwsh_json};

const SPOOLER_CHECK: &str = r##"
$spooler = Get-Service -Name Spooler -ErrorAction SilentlyContinue
$out = [ordered]@{
    spooler = if ($spooler) { [string]$spooler.Status } else { 'NotInstalled' }
    spooler_start_type = if ($spooler) { [string]$spooler.StartType } else { $null }
    error = $null
}
"##;

const PRINTERS: &str = r##"
@@SPOOLER@@
$out.printers = @()
if ($spooler -and $spooler.Status -eq 'Running') {
    try {
        $wmi = @{}
        foreach ($w in @(Get-CimInstance Win32_Printer -ErrorAction SilentlyContinue)) { $wmi[[string]$w.Name] = $w }
        $out.printers = @(Get-Printer -ErrorAction Stop | Sort-Object Name | ForEach-Object {
            $w = $wmi[[string]$_.Name]
            [ordered]@{
                name = [string]$_.Name
                default = if ($w) { [bool]$w.Default } else { $false }
                status = [string]$_.PrinterStatus
                work_offline = if ($w) { [bool]$w.WorkOffline } else { $null }
                type = [string]$_.Type
                driver = [string]$_.DriverName
                port = [string]$_.PortName
                shared = [bool]$_.Shared
                share_name = if ($_.Shared) { [string]$_.ShareName } else { $null }
                location = [string]$_.Location
                comment = [string]$_.Comment
                queued_jobs = [int]$_.JobCount
            }
        })
    } catch {
        $out.error = $_.Exception.Message
    }
} else {
    $out.error = 'The Print Spooler service is not running, so Windows cannot list printers.'
}
$out | ConvertTo-Json -Compress -Depth 4
"##;

const JOBS_TEMPLATE: &str = r##"
@@SPOOLER@@
$printer = @@PRINTER@@
$max = @@MAX@@
$out.printer = if ($printer) { $printer } else { $null }
$out.jobs = @()
if ($spooler -and $spooler.Status -eq 'Running') {
    try {
        $names = if ($printer) { @((Get-Printer -Name $printer -ErrorAction Stop).Name) } else { @(Get-Printer -ErrorAction Stop | ForEach-Object { $_.Name }) }
        $jobs = foreach ($n in $names) {
            Get-PrintJob -PrinterName $n -ErrorAction SilentlyContinue | ForEach-Object {
                [ordered]@{
                    printer = [string]$n
                    id = [int]$_.Id
                    document = [string]$_.DocumentName
                    owner = [string]$_.UserName
                    status = [string]$_.JobStatus
                    submitted_utc = if ($_.SubmittedTime) { $_.SubmittedTime.ToUniversalTime().ToString("o") } else { $null }
                    pages_total = [int]$_.TotalPages
                    pages_printed = [int]$_.PagesPrinted
                    size_bytes = [int64]$_.Size
                    position = [int]$_.Position
                }
            }
        }
        $out.jobs = @($jobs | Select-Object -First $max)
    } catch {
        $out.error = $_.Exception.Message
    }
} else {
    $out.error = 'The Print Spooler service is not running, so Windows cannot list print jobs.'
}
$out.count = @($out.jobs).Count
$out | ConvertTo-Json -Compress -Depth 4
"##;

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/print.json"),
        vec![
            ("print.list", printers_handler()),
            ("print.jobs", handler(print_jobs)),
        ],
    );
}

fn printers_handler() -> crate::dispatch::Handler {
    static SCRIPT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    json_script_handler(SCRIPT.get_or_init(|| PRINTERS.replace("@@SPOOLER@@", SPOOLER_CHECK)))
}

pub fn jobs_script(args: &Args) -> Result<String, HandlerError> {
    let printer = string_arg(args, "printer", "").trim().to_string();
    if printer.len() > 256 || printer.chars().any(char::is_control) {
        return Err(HandlerError::plain(
            "print.jobs: 'printer' must be a printer name of at most 256 characters",
        ));
    }
    let max = int_arg(args, "max_results", 200).clamp(1, 1000);
    Ok(fill(
        JOBS_TEMPLATE,
        &[
            ("@@SPOOLER@@", SPOOLER_CHECK),
            ("@@PRINTER@@", &ps_single_quote(&printer)),
            ("@@MAX@@", &max.to_string()),
        ],
    ))
}

async fn print_jobs(ctx: Ctx, args: Args) -> CapResult {
    let script = jobs_script(&args)?;
    run_pwsh_json(&ctx.cancel, &script)
        .await
        .map_err(HandlerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn registers_the_print_kinds_with_time_limits() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["print.jobs", "print.list"]);
        for spec in r.specs() {
            assert!(!spec.label.is_empty() && !spec.risky, "{}", spec.kind);
            assert!(
                spec.default_timeout_ms > 0,
                "{} has no time limit",
                spec.kind
            );
        }
    }

    #[test]
    fn jobs_script_quotes_the_printer_and_clamps() {
        let script = jobs_script(&args(
            json!({"printer": " Bob's HP ", "max_results": 99999}),
        ))
        .unwrap();
        assert!(script.contains("$printer = 'Bob''s HP'"));
        assert!(script.contains("$max = 1000"));
        assert!(script.contains("Get-Service -Name Spooler"));
        assert!(!script.contains("@@"));
        let script = jobs_script(&Args::new()).unwrap();
        assert!(script.contains("$printer = ''") && script.contains("$max = 200"));
    }

    #[test]
    fn jobs_script_rejects_odd_printer_names() {
        assert!(jobs_script(&args(json!({"printer": "a\nb"}))).is_err());
        assert!(jobs_script(&args(json!({"printer": "x".repeat(300)}))).is_err());
    }

    #[test]
    fn printer_script_has_no_placeholders_left() {
        let script = PRINTERS.replace("@@SPOOLER@@", SPOOLER_CHECK);
        assert!(!script.contains("@@") && script.contains("ConvertTo-Json -Compress"));
    }
}

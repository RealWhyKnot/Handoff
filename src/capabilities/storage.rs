use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::proc::{fill, load_specs, string_arg};
use super::pwsh::{ps_single_quote, run_pwsh_json};

const TEMPLATE: &str = r##"
$driveLetter = @@DRIVE@@
if ($driveLetter) {
    $volumes = @(Get-Volume -DriveLetter $driveLetter -ErrorAction SilentlyContinue)
} else {
    $volumes = @(Get-Volume -ErrorAction SilentlyContinue)
}

$entries = $volumes |
    Sort-Object DriveLetter, FileSystemLabel |
    ForEach-Object {
        $sizeGB = $null
        $freeGB = $null
        $freePercent = $null
        if ($null -ne $_.Size) {
            $sizeGB = [math]::Round([double]$_.Size / 1GB, 2)
        }
        if ($null -ne $_.SizeRemaining) {
            $freeGB = [math]::Round([double]$_.SizeRemaining / 1GB, 2)
        }
        if ($null -ne $_.Size -and $_.Size -gt 0 -and $null -ne $_.SizeRemaining) {
            $freePercent = [math]::Round(([double]$_.SizeRemaining / [double]$_.Size) * 100, 1)
        }
        [ordered]@{
            drive_letter = if ($null -ne $_.DriveLetter) { [string]$_.DriveLetter } else { $null }
            label = [string]$_.FileSystemLabel
            file_system = [string]$_.FileSystem
            size_gb = $sizeGB
            free_gb = $freeGB
            free_percent = $freePercent
            health_status = [string]$_.HealthStatus
            operational_status = [string]$_.OperationalStatus
            path = if ($_.Path) { [string]$_.Path } else { $null }
            unique_id = if ($_.UniqueId) { [string]$_.UniqueId } else { $null }
        }
    }

[ordered]@{
    drive_letter = $driveLetter
    count = @($entries).Count
    entries = @($entries)
} | ConvertTo-Json -Compress -Depth 4
"##;

pub fn register(r: &mut Router) {
    for spec in load_specs(include_str!("specs/storage.json")) {
        r.register_spec(spec, handler(storage_volumes));
    }
}

pub fn drive_letter_arg(args: &Args) -> Result<String, HandlerError> {
    let raw = string_arg(args, "drive_letter", "");
    let trimmed = raw.trim();
    let letter = trimmed.strip_suffix(':').unwrap_or(trimmed);
    if letter.is_empty() {
        return Ok(String::new());
    }
    if letter.len() != 1 {
        return Err(HandlerError::plain(
            "storage.volumes: drive_letter must be a single letter",
        ));
    }
    if !letter.as_bytes()[0].is_ascii_alphabetic() {
        return Err(HandlerError::plain(
            "storage.volumes: drive_letter must be A-Z",
        ));
    }
    Ok(letter.to_uppercase())
}

async fn storage_volumes(ctx: Ctx, args: Args) -> CapResult {
    let letter = drive_letter_arg(&args)?;
    let script = fill(TEMPLATE, &[("@@DRIVE@@", &ps_single_quote(&letter))]);
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
    fn registers_storage_volumes() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["storage.volumes"]);
    }

    #[test]
    fn drive_letter_is_normalized_and_bad_input_rejected() {
        assert_eq!(
            drive_letter_arg(&args(json!({"drive_letter": " c: "}))).unwrap(),
            "C"
        );
        assert_eq!(drive_letter_arg(&Args::new()).unwrap(), "");
        assert_eq!(
            drive_letter_arg(&args(json!({"drive_letter": ":"}))).unwrap(),
            ""
        );
        let err = drive_letter_arg(&args(json!({"drive_letter": "System"}))).unwrap_err();
        assert!(err.message.contains("single letter"), "{}", err.message);
        let err = drive_letter_arg(&args(json!({"drive_letter": "1"}))).unwrap_err();
        assert!(err.message.contains("A-Z"), "{}", err.message);
    }
}

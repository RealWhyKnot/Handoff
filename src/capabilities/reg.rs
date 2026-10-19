use crate::dispatch::{handler, Args, CapResult, Ctx, HandlerError, Router};

use super::proc::{bool_arg, fill, int_arg, string_arg};
use super::pwsh::ps_single_quote;
use super::pwsh_out::run_pwsh_json;

const TEMPLATE: &str = r##"
$hive = @@HIVE@@
$key  = @@KEY@@
$value = @@VALUE@@
$recursive = $@@RECURSIVE@@
$max = @@MAX@@

$root = "$hive`:\$key"
if (-not (Test-Path -LiteralPath $root)) {
    [ordered]@{
        error = "key not found"
        path  = $root
    } | ConvertTo-Json -Compress
    return
}

function Get-RegEntry($psPath) {
    $item = Get-Item -LiteralPath $psPath -ErrorAction Stop
    $entry = [ordered]@{
        path     = $psPath
        subkeys  = @($item.GetSubKeyNames())
        values   = @()
    }
    foreach ($n in $item.GetValueNames()) {
        $kind = $item.GetValueKind($n).ToString()
        $data = $item.GetValue($n)
        if ($data -is [byte[]]) {
            $data = [Convert]::ToBase64String($data)
            $kind = "$kind+base64"
        } elseif ($data -is [string[]]) {
            $data = @($data)
        }
        $entry.values += [ordered]@{
            name = $n
            kind = $kind
            data = $data
        }
    }
    return $entry
}

if ($value) {
    $item = Get-Item -LiteralPath $root -ErrorAction SilentlyContinue
    if (-not $item -or -not ($item.GetValueNames() -contains $value)) {
        [ordered]@{
            error = "value not found"
            path  = $root
            name  = $value
        } | ConvertTo-Json -Compress
        return
    }
    $kind = $item.GetValueKind($value).ToString()
    $data = $item.GetValue($value)
    if ($data -is [byte[]]) {
        $data = [Convert]::ToBase64String($data)
        $kind = "$kind+base64"
    } elseif ($data -is [string[]]) {
        $data = @($data)
    }
    [ordered]@{
        path = $root
        name = $value
        kind = $kind
        data = $data
    } | ConvertTo-Json -Compress -Depth 6
    return
}

if (-not $recursive) {
    Get-RegEntry $root | ConvertTo-Json -Compress -Depth 6
    return
}

$visited = New-Object System.Collections.Generic.List[object]
$stack = New-Object System.Collections.Stack
$stack.Push($root)
while ($stack.Count -gt 0 -and $visited.Count -lt $max) {
    $current = $stack.Pop()
    try {
        $entry = Get-RegEntry $current
        $visited.Add($entry) | Out-Null
        foreach ($sub in $entry.subkeys) {
            if ($visited.Count + $stack.Count -ge $max) { break }
            $stack.Push(($current.TrimEnd('\') + '\' + $sub))
        }
    } catch {
    }
}
[ordered]@{
    path     = $root
    count    = $visited.Count
    max      = $max
    entries  = @($visited)
} | ConvertTo-Json -Compress -Depth 6
"##;

const HIVES: [&str; 5] = ["HKLM", "HKCU", "HKCR", "HKU", "HKCC"];

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/reg.json"),
        vec![("reg.query", handler(reg_query))],
    );
}

fn key_char_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || " _-.\\/()[]@:".contains(c)
}

pub fn query_script(args: &Args) -> Result<String, HandlerError> {
    let hive = string_arg(args, "hive", "").trim().to_uppercase();
    let key = string_arg(args, "key", "");
    let value = string_arg(args, "value", "");
    let recursive = bool_arg(args, "recursive", false);
    let mut max = int_arg(args, "max_results", 0);

    let hive = if hive.is_empty() {
        "HKLM".to_string()
    } else {
        hive
    };
    if !HIVES.contains(&hive.as_str()) {
        return Err(HandlerError::plain(
            "reg.query: hive must be one of HKLM, HKCU, HKCR, HKU, HKCC",
        ));
    }

    let key = key.trim().trim_start_matches(['\\', '/']).to_string();
    if key.is_empty() {
        return Err(HandlerError::plain("reg.query: 'key' is required"));
    }
    if key.len() > 512 {
        return Err(HandlerError::plain("reg.query: 'key' is too long"));
    }
    if !key.chars().all(key_char_allowed) {
        return Err(HandlerError::plain(
            "reg.query: 'key' contains unsupported characters",
        ));
    }
    if key.contains("..") {
        return Err(HandlerError::plain("reg.query: 'key' may not contain '..'"));
    }

    let value = value.trim().to_string();
    if value.len() > 256 {
        return Err(HandlerError::plain("reg.query: 'value' is too long"));
    }

    if max <= 0 {
        max = 200;
    }
    if max > 2000 {
        max = 2000;
    }

    Ok(fill(
        TEMPLATE,
        &[
            ("@@HIVE@@", &ps_single_quote(&hive)),
            ("@@KEY@@", &ps_single_quote(&key)),
            ("@@VALUE@@", &ps_single_quote(&value)),
            ("@@RECURSIVE@@", if recursive { "true" } else { "false" }),
            ("@@MAX@@", &max.to_string()),
        ],
    ))
}

async fn reg_query(ctx: Ctx, args: Args) -> CapResult {
    let script = query_script(&args)?;
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
    fn registers_reg_query_with_a_full_spec() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["reg.query"]);
        let spec = r.spec_for("reg.query").unwrap();
        assert_eq!(spec.label, "Query registry");
        assert!(spec.params.iter().any(|p| p.name == "key" && p.required));
    }

    #[test]
    fn rejects_a_bad_hive() {
        let err = query_script(&args(json!({"hive": "HKBOGUS", "key": "SOFTWARE"}))).unwrap_err();
        assert!(err.message.contains("hive must be"), "{}", err.message);
    }

    #[test]
    fn rejects_a_missing_key() {
        let err = query_script(&args(json!({"hive": "HKLM"}))).unwrap_err();
        assert!(err.message.contains("'key' is required"), "{}", err.message);
        let err = query_script(&args(json!({"key": " \\/ "}))).unwrap_err();
        assert!(err.message.contains("'key' is required"), "{}", err.message);
    }

    #[test]
    fn rejects_path_traversal() {
        let err = query_script(&args(
            json!({"hive": "HKLM", "key": "SOFTWARE\\..\\SECRET"}),
        ))
        .unwrap_err();
        assert!(err.message.contains(".."), "{}", err.message);
    }

    #[test]
    fn rejects_bad_characters() {
        let err = query_script(&args(json!({"hive": "HKLM", "key": "SOFTWARE;DROP"}))).unwrap_err();
        assert!(
            err.message.contains("unsupported characters"),
            "{}",
            err.message
        );
        let err = query_script(&args(json!({"key": "SOFTWARE\u{e9}"}))).unwrap_err();
        assert!(
            err.message.contains("unsupported characters"),
            "{}",
            err.message
        );
    }

    #[test]
    fn rejects_over_long_key_and_value() {
        let err = query_script(&args(json!({"key": "a".repeat(513)}))).unwrap_err();
        assert!(err.message.contains("'key' is too long"), "{}", err.message);
        let err = query_script(&args(json!({"key": "a", "value": "v".repeat(257)}))).unwrap_err();
        assert!(
            err.message.contains("'value' is too long"),
            "{}",
            err.message
        );
    }

    #[test]
    fn normalizes_and_quotes_the_arguments() {
        let script = query_script(&args(json!({
            "hive": " hkcu ",
            "key": "\\Software\\Microsoft (x86)\\Co",
            "value": " Val'ue ",
            "recursive": true,
            "max_results": 99999
        })))
        .unwrap();
        assert!(script.contains("$hive = 'HKCU'"));
        assert!(script.contains("$key  = 'Software\\Microsoft (x86)\\Co'"));
        assert!(script.contains("$value = 'Val''ue'"));
        assert!(script.contains("$recursive = $true"));
        assert!(script.contains("$max = 2000"));
        assert!(script.contains("$root = \"$hive`:\\$key\""));
        assert!(!script.contains("@@"));
    }

    #[test]
    fn defaults_to_hklm_and_two_hundred_rows() {
        let script = query_script(&args(json!({"key": "SOFTWARE"}))).unwrap();
        assert!(script.contains("$hive = 'HKLM'"));
        assert!(script.contains("$recursive = $false"));
        assert!(script.contains("$max = 200"));
    }
}

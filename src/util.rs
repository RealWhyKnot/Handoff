use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn format_utc_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub fn format_utc_day(ms: i64) -> String {
    let (y, mo, d) = civil_from_days(ms.div_euclid(1000).div_euclid(86_400));
    format!("{y:04}-{mo:02}-{d:02}")
}

pub fn utc_timestamp() -> String {
    format_utc_ms(now_ms())
}

pub fn utc_day() -> String {
    format_utc_day(now_ms())
}

pub fn first_non_empty(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| v.trim())
        .find(|v| !v.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

pub fn user_config_dir() -> Option<PathBuf> {
    user_config_dir_from(&env_var)
}

pub fn user_config_dir_from(env: &dyn Fn(&str) -> String) -> Option<PathBuf> {
    if cfg!(windows) {
        let dir = env("APPDATA");
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
    let xdg = env("XDG_CONFIG_HOME");
    if !xdg.is_empty() {
        return Some(PathBuf::from(xdg));
    }
    let home = env("HOME");
    (!home.is_empty()).then(|| PathBuf::from(home).join(".config"))
}

pub fn user_cache_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        let dir = env_var("LOCALAPPDATA");
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
    let xdg = env_var("XDG_CACHE_HOME");
    if !xdg.is_empty() {
        return Some(PathBuf::from(xdg));
    }
    let home = env_var("HOME");
    (!home.is_empty()).then(|| PathBuf::from(home).join(".cache"))
}

pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn look_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) && std::path::Path::new(name).extension().is_none() {
        let pathext = env_var("PATHEXT");
        let pathext = if pathext.is_empty() {
            ".COM;.EXE;.BAT;.CMD".to_string()
        } else {
            pathext
        };
        pathext
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_string())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
pub fn hostname() -> String {
    use windows::core::PWSTR;
    use windows::Win32::System::WindowsProgramming::GetComputerNameW;

    let mut buf = [0u16; 64];
    let mut len = buf.len() as u32;
    let ok = unsafe { GetComputerNameW(Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    match ok {
        Ok(()) => String::from_utf16_lossy(&buf[..len as usize]),
        Err(_) => env_var("COMPUTERNAME"),
    }
}

#[cfg(not(windows))]
pub fn hostname() -> String {
    let name = env_var("HOSTNAME");
    if name.is_empty() {
        "localhost".to_string()
    } else {
        name
    }
}

pub fn go_float_string(f: f64) -> String {
    if f == 0.0 {
        return "0".to_string();
    }
    if !f.is_finite() {
        return if f.is_nan() {
            "NaN".to_string()
        } else if f > 0.0 {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        };
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..6).contains(&exp) {
        return format!("{f}");
    }
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exp.abs())
}

pub fn go_value_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "<nil>".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) if n.is_f64() => go_float_string(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn go_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{b}' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn go_json_escape_html(s: &str) -> String {
    s.replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

pub fn go_pretty_json(v: &serde_json::Value) -> String {
    go_json_escape_html(&serde_json::to_string_pretty(v).unwrap_or_default())
}

pub fn parse_go_duration(input: &str) -> Option<std::time::Duration> {
    let mut rest = input;
    let mut negative = false;
    if let Some(r) = rest.strip_prefix('-') {
        negative = true;
        rest = r;
    } else if let Some(r) = rest.strip_prefix('+') {
        rest = r;
    }
    if rest == "0" {
        return Some(std::time::Duration::ZERO);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total: u128 = 0;
    while !rest.is_empty() {
        let int_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let int_part: u128 = if int_len == 0 {
            0
        } else {
            rest[..int_len].parse().ok()?
        };
        rest = &rest[int_len..];
        let mut frac_digits = "";
        let mut had_frac = false;
        if let Some(r) = rest.strip_prefix('.') {
            let frac_len = r.bytes().take_while(u8::is_ascii_digit).count();
            frac_digits = &r[..frac_len];
            rest = &r[frac_len..];
            had_frac = true;
        }
        if int_len == 0 && !had_frac {
            return None;
        }
        if int_len == 0 && frac_digits.is_empty() {
            return None;
        }
        let unit_len = rest
            .char_indices()
            .find(|(_, c)| c.is_ascii_digit() || *c == '.')
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let unit: u128 = match &rest[..unit_len] {
            "ns" => 1,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60 * 1_000_000_000,
            "h" => 3600 * 1_000_000_000,
            _ => return None,
        };
        rest = &rest[unit_len..];
        let mut nanos = int_part.checked_mul(unit)?;
        if !frac_digits.is_empty() {
            let digits = &frac_digits[..frac_digits.len().min(18)];
            let frac: u128 = digits.parse().ok()?;
            nanos += frac * unit / 10u128.pow(digits.len() as u32);
        }
        total = total.checked_add(nanos)?;
    }
    if negative {
        return Some(std::time::Duration::ZERO);
    }
    Some(std::time::Duration::from_nanos(u64::try_from(total).ok()?))
}

fn fmt_frac(v: u128, prec: u32) -> String {
    let div = 10u128.pow(prec);
    let int = v / div;
    let frac = v % div;
    if frac == 0 {
        return int.to_string();
    }
    let frac = format!("{frac:0width$}", width = prec as usize);
    format!("{int}.{}", frac.trim_end_matches('0'))
}

pub fn go_duration_string(d: std::time::Duration) -> String {
    let nanos = d.as_nanos();
    if nanos == 0 {
        return "0s".to_string();
    }
    if nanos < 1_000_000_000 {
        return if nanos < 1_000 {
            format!("{nanos}ns")
        } else if nanos < 1_000_000 {
            format!("{}\u{b5}s", fmt_frac(nanos, 3))
        } else {
            format!("{}ms", fmt_frac(nanos, 6))
        };
    }
    let secs = fmt_frac(nanos % 60_000_000_000, 9);
    let total_minutes = nanos / 60_000_000_000;
    let mut out = format!("{secs}s");
    if total_minutes > 0 {
        out = format!("{}m{out}", total_minutes % 60);
        if total_minutes >= 60 {
            out = format!("{}h{out}", total_minutes / 60);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_like_the_audit_log_expects() {
        assert_eq!(format_utc_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_utc_ms(1_792_382_498_739), "2026-10-19T04:01:38.739Z");
        assert_eq!(format_utc_day(1_792_382_498_739), "2026-10-19");
        assert_eq!(format_utc_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn go_float_formatting_matches_fmt_v() {
        assert_eq!(go_float_string(3.0), "3");
        assert_eq!(go_float_string(100_000.0), "100000");
        assert_eq!(go_float_string(1_234_567.0), "1.234567e+06");
        assert_eq!(go_float_string(123_456_789.0), "1.23456789e+08");
        assert_eq!(go_float_string(0.0001), "0.0001");
        assert_eq!(go_float_string(0.00001), "1e-05");
        assert_eq!(go_float_string(12.5), "12.5");
        assert_eq!(go_float_string(-5.0), "-5");
        assert_eq!(go_float_string(1e20), "1e+20");
    }

    #[test]
    fn first_non_empty_trims_and_skips_blanks() {
        assert_eq!(first_non_empty(&["", "  ", " x "]), "x");
        assert_eq!(first_non_empty(&["", " "]), "");
    }

    #[test]
    fn go_durations_round_trip_like_the_flag_package() {
        use std::time::Duration;
        assert_eq!(parse_go_duration("60s"), Some(Duration::from_secs(60)));
        assert_eq!(parse_go_duration("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_go_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_go_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_go_duration("5"), None);
        assert_eq!(parse_go_duration("abc"), None);
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(go_duration_string(Duration::from_secs(60)), "1m0s");
        assert_eq!(go_duration_string(Duration::from_secs(3600)), "1h0m0s");
        assert_eq!(go_duration_string(Duration::from_millis(1500)), "1.5s");
        assert_eq!(go_duration_string(Duration::from_millis(250)), "250ms");
        assert_eq!(go_duration_string(Duration::ZERO), "0s");
        assert_eq!(go_duration_string(Duration::from_secs(5)), "5s");
    }

    #[test]
    fn go_quote_matches_percent_q() {
        assert_eq!(go_quote("a b"), "\"a b\"");
        assert_eq!(go_quote("C:\\Temp"), "\"C:\\\\Temp\"");
        assert_eq!(go_quote("x\ny"), "\"x\\ny\"");
    }
}

use super::fs::err;
use crate::dispatch::HandlerError;

fn is_slash(b: u8) -> bool {
    b == b'/' || b == b'\\'
}

pub(crate) fn volume_len(p: &str) -> usize {
    let b = p.as_bytes();
    let l = b.len();
    if l < 2 {
        return 0;
    }
    if b[1] == b':' && b[0].is_ascii_alphabetic() {
        return 2;
    }
    if l >= 5 && is_slash(b[0]) && is_slash(b[1]) && !is_slash(b[2]) && b[2] != b'.' {
        let mut n = 3;
        while n < l - 1 {
            if is_slash(b[n]) {
                n += 1;
                if !is_slash(b[n]) {
                    if b[n] == b'.' {
                        break;
                    }
                    while n < l {
                        if is_slash(b[n]) {
                            break;
                        }
                        n += 1;
                    }
                    return n;
                }
                break;
            }
            n += 1;
        }
    }
    0
}

pub(crate) fn go_clean(original: &str) -> String {
    if original.is_empty() {
        return ".".to_string();
    }
    let vol_len = volume_len(original);
    let vol = &original[..vol_len];
    let path = &original[vol_len..];
    if path.is_empty() {
        let b = original.as_bytes();
        if vol_len > 1 && is_slash(b[0]) && is_slash(b[1]) {
            return original.replace('/', "\\");
        }
        return format!("{original}.");
    }
    let rooted = is_slash(path.as_bytes()[0]);
    let mut parts: Vec<&str> = Vec::new();
    for el in path.split(['/', '\\']) {
        match el {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = vol.replace('/', "\\");
    if rooted {
        out.push('\\');
    }
    if parts.is_empty() && !rooted {
        out.push('.');
    }
    out.push_str(&parts.join("\\"));
    out
}

const DEVICE_NAMES: [&str; 8] = [
    "con", "prn", "aux", "nul", "conin$", "conout$", "clock$", "config$",
];

fn is_device_name(segment: &str) -> bool {
    let stem = segment
        .split('.')
        .next()
        .unwrap_or(segment)
        .trim_end_matches(' ')
        .to_lowercase();
    if DEVICE_NAMES.contains(&stem.as_str()) {
        return true;
    }
    let mut chars = stem.chars();
    let head: String = chars.by_ref().take(3).collect();
    let tail: Vec<char> = chars.collect();
    (head == "com" || head == "lpt")
        && tail.len() == 1
        && (tail[0].is_ascii_digit() || matches!(tail[0], '\u{b9}' | '\u{b2}' | '\u{b3}'))
}

pub(crate) fn local_path(kind: &str, raw: &str) -> Result<String, HandlerError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(err(format!("{kind}: 'path' is required")));
    }
    let b = raw.as_bytes();
    if b.len() >= 2 && is_slash(b[0]) && is_slash(b[1]) {
        return Err(err(format!(
            "{kind}: network and device paths are not allowed"
        )));
    }
    if !(b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && is_slash(b[2])) {
        return Err(err(format!(
            "{kind}: path must be absolute, like C:\\folder"
        )));
    }
    let path = go_clean(raw);
    let rest = &path[2..];
    if rest.contains(':') {
        return Err(err(format!(
            "{kind}: ':' is only allowed after the drive letter"
        )));
    }
    if let Some(dev) = rest.split('\\').find(|s| is_device_name(s)) {
        return Err(err(format!("{kind}: {dev} is a reserved device name")));
    }
    Ok(path)
}

pub(crate) fn go_base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let mut p = path;
    while p.as_bytes().last().is_some_and(|b| is_slash(*b)) {
        p = &p[..p.len() - 1];
    }
    p = &p[volume_len(p)..];
    if let Some(i) = p.rfind(['/', '\\']) {
        p = &p[i + 1..];
    }
    if p.is_empty() {
        return "\\".to_string();
    }
    p.to_string()
}

pub(super) fn os_basename(name: &str) -> String {
    let mut n = name;
    while n.len() > 1 && n.as_bytes().last().is_some_and(|b| is_slash(*b)) {
        n = &n[..n.len() - 1];
    }
    let b = n.as_bytes();
    if b.len() == 2 && b[1] == b':' {
        return ".".to_string();
    }
    if b.len() > 2 && b[1] == b':' {
        n = &n[2..];
    }
    match n.rfind(['/', '\\']) {
        Some(i) => n[i + 1..].to_string(),
        None => n.to_string(),
    }
}

pub(crate) fn go_join(parent: &str, name: &str) -> String {
    go_clean(&format!("{parent}\\{name}"))
}

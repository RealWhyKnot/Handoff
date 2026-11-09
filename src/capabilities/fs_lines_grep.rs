use super::fs::bool_arg;
use super::fs::err;
use super::fs::int_arg;
use super::fs::str_arg;
use super::fs_lines::CANCELLED;
use super::fs_lines::MAX_LINES;
use super::fs_lines::REPLY_CAP;
use super::fs_lines_reader::LineReader;
use super::fs_lines_reader::open;
use super::fs_lines_reply::cut_note;
use super::fs_lines_reply::json_len;
use super::fs_lines_reply::show;
use crate::dispatch::{Args, CapResult};
use serde_json::{Value, json};
use std::collections::VecDeque;
use tokio_util::sync::CancellationToken;

const LINE_KEEP: usize = 1 << 20;

const MAX_MATCHES: i64 = 1000;

const MAX_CONTEXT: i64 = 50;

enum Matcher {
    Exact(String),
    Ascii(Vec<u8>),
    Fold(String),
}

impl Matcher {
    fn new(text: &str, case_sensitive: bool) -> Matcher {
        if case_sensitive {
            Matcher::Exact(text.to_string())
        } else if text.is_ascii() {
            Matcher::Ascii(text.to_ascii_lowercase().into_bytes())
        } else {
            Matcher::Fold(text.to_lowercase())
        }
    }

    fn find(&self, line: &str) -> Option<usize> {
        match self {
            Matcher::Exact(t) => line.find(t.as_str()),
            Matcher::Ascii(t) => {
                let h = line.as_bytes();
                if t.is_empty() || t.len() > h.len() {
                    return None;
                }
                let first = t[0];
                (0..=h.len() - t.len()).find(|&i| {
                    h[i].to_ascii_lowercase() == first && h[i..i + t.len()].eq_ignore_ascii_case(t)
                })
            }
            Matcher::Fold(t) => {
                let lower = line.to_lowercase();
                let i = lower.find(t.as_str())?;
                let chars = lower[..i].chars().count();
                Some(
                    line.char_indices()
                        .nth(chars)
                        .map(|(b, _)| b)
                        .unwrap_or(line.len()),
                )
            }
        }
    }
}

pub(crate) fn grep(args: &Args, cancel: &CancellationToken) -> CapResult {
    let mut o = open("fs.grep", args)?;
    let text = str_arg(args, "text").unwrap_or_default();
    if text.is_empty() {
        return Err(err("fs.grep: 'text' is required"));
    }
    let case_sensitive = bool_arg(args, "case_sensitive").unwrap_or(false);
    let before =
        usize::try_from(int_arg(args, "before").unwrap_or(0).clamp(0, MAX_CONTEXT)).unwrap_or(0);
    let after =
        usize::try_from(int_arg(args, "after").unwrap_or(0).clamp(0, MAX_CONTEXT)).unwrap_or(0);
    let max_matches = match int_arg(args, "max_matches").unwrap_or(100) {
        m if m <= 0 => 100,
        m => crate::util::positive_u64(m.min(MAX_MATCHES)),
    };
    let start = crate::util::positive_u64(int_arg(args, "start_line").unwrap_or(1).max(1));
    let matcher = Matcher::new(&text, case_sensitive);
    let context = before > 0 || after > 0;

    let enc = o.enc;
    o.seek(enc.bom_len())?;
    let mut rd = LineReader::new(&o.file, enc, cancel);
    let mut raw = Vec::new();
    let mut ring: VecDeque<(u64, String, bool)> = VecDeque::with_capacity(before + 1);
    let mut out: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut match_lines: Vec<u64> = Vec::new();
    let mut total = 0u64;
    let mut listing = true;
    let mut capped = false;
    let mut after_left = 0usize;
    let mut last_out = 0u64;
    let mut long = 0u64;
    let mut cut = 0u64;
    let mut n = 0u64;
    loop {
        let k = if n + 1 + crate::util::count_u64(before) < start {
            0
        } else {
            LINE_KEEP
        };
        let Some(c) = rd.next_line(&mut raw, k).map_err(o.io("read"))? else {
            break;
        };
        n += 1;
        if k == 0 {
            continue;
        }
        let line = enc.decode(&raw);
        let hit = if n >= start {
            if c {
                long += 1;
            }
            matcher.find(&line)
        } else {
            None
        };
        match hit {
            Some(at) => {
                total += 1;
                if !listing {
                    continue;
                }
                if crate::util::count_u64(match_lines.len()) >= max_matches {
                    listing = false;
                    capped = true;
                    continue;
                }
                let mut prev = last_out;
                let mut add: Vec<String> = Vec::new();
                let mut add_cut = 0u64;
                for (ln, s, was_cut) in ring.drain(..) {
                    if ln <= prev {
                        continue;
                    }
                    if context && prev > 0 && ln > prev + 1 {
                        add.push("--".to_string());
                    }
                    add.push(format!("{ln}- {s}"));
                    add_cut += u64::from(was_cut);
                    prev = ln;
                }
                let (s, was_cut) = show(&line, at, c);
                if context && prev > 0 && n > prev + 1 {
                    add.push("--".to_string());
                }
                add.push(format!("{n}: {s}"));
                add_cut += u64::from(was_cut);
                let size: usize = add.iter().map(|l| json_len(l)).sum();
                if out.len() + add.len() > crate::util::size_usize(MAX_LINES)
                    || bytes + size > REPLY_CAP
                {
                    listing = false;
                    capped = true;
                    continue;
                }
                out.extend(add);
                bytes += size;
                cut += add_cut;
                last_out = n;
                match_lines.push(n);
                after_left = after;
            }
            None => {
                if !listing {
                    continue;
                }
                if after_left > 0 && n >= start {
                    after_left -= 1;
                    let (s, was_cut) = show(&line, 0, c);
                    let l = format!("{n}- {s}");
                    let size = json_len(&l);
                    if out.len() + 1 > crate::util::size_usize(MAX_LINES)
                        || bytes + size > REPLY_CAP
                    {
                        listing = false;
                        capped = true;
                        continue;
                    }
                    out.push(l);
                    bytes += size;
                    cut += u64::from(was_cut);
                    last_out = n;
                } else if before > 0 {
                    let (s, was_cut) = show(&line, 0, c);
                    ring.push_back((n, s, was_cut));
                    if ring.len() > before {
                        ring.pop_front();
                    }
                }
            }
        }
    }

    let listed = crate::util::count_u64(match_lines.len());
    let mut notes: Vec<String> = Vec::new();
    if rd.cancelled {
        notes.push(CANCELLED.to_string());
    }
    if capped {
        let resume = match_lines.last().map(|l| l + 1).unwrap_or(start);
        notes.push(format!(
            "listed {listed} of {total} matches; search again with start_line {resume} for more"
        ));
    }
    if cut > 0 {
        notes.push(cut_note(cut));
    }
    if long == 1 {
        notes.push("1 line over 1 MiB was only searched in its first 1 MiB".to_string());
    } else if long > 1 {
        notes.push(format!(
            "{long} lines over 1 MiB were only searched in their first 1 MiB"
        ));
    }

    let truncated = rd.cancelled || capped || cut > 0 || long > 0;
    let mut m = o.base("grep");
    m.insert("text".into(), json!(text));
    m.insert("case_sensitive".into(), json!(case_sensitive));
    m.insert("start_line".into(), json!(start));
    m.insert("before".into(), json!(before));
    m.insert("after".into(), json!(after));
    m.insert("max_matches".into(), json!(max_matches));
    m.insert("match_count".into(), json!(total));
    m.insert("listed".into(), json!(listed));
    m.insert("match_lines".into(), json!(match_lines));
    m.insert("lines_scanned".into(), json!(n));
    m.insert("line_count".into(), json!(out.len()));
    m.insert("lines".into(), json!(out));
    m.insert("truncated".into(), json!(truncated));
    if !notes.is_empty() {
        m.insert("note".into(), json!(notes.join("; ")));
    }
    Ok(Value::Object(m))
}

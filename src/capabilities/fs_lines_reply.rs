use super::fs::int_arg;
use super::fs_lines::LINE_SHOW;
use super::fs_lines::MAX_LINES;
use crate::dispatch::Args;

pub(super) fn show(text: &str, at: usize, cut: bool) -> (String, bool) {
    if text.len() <= LINE_SHOW && !cut {
        return (text.to_string(), false);
    }
    let mut start = 0;
    if text.len() > LINE_SHOW && at + 64 > LINE_SHOW {
        start = at.min(text.len()) - (LINE_SHOW / 4).min(at.min(text.len()));
    }
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + LINE_SHOW).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut s = String::with_capacity(end - start + 6);
    if start > 0 {
        s.push_str("...");
    }
    s.push_str(&text[start..end]);
    if end < text.len() || cut {
        s.push_str("...");
    }
    (s, true)
}

pub(super) fn json_len(s: &str) -> usize {
    let extra: usize = s
        .bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 1,
            0..=0x1f => 5,
            _ => 0,
        })
        .sum();
    s.len() + extra + 3
}

pub(super) fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

pub(super) fn cut_note(n: u64) -> String {
    let kib = LINE_SHOW / 1024;
    if n == 1 {
        format!("1 long line was cut to {kib} KiB")
    } else {
        format!("{n} long lines were cut to {kib} KiB")
    }
}

pub(super) fn line_count_arg(args: &Args) -> u64 {
    let n = int_arg(args, "lines").unwrap_or(80);
    if n <= 0 {
        80
    } else {
        crate::util::positive_u64(n).min(MAX_LINES)
    }
}

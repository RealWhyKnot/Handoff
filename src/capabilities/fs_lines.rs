use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};

use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::dispatch::{Args, CapResult, HandlerError};

use super::fs::{
    bool_arg, err, fmt_time, go_lossy, guard_read_path, int_arg, local_path, read_err, stat,
    str_arg,
};

pub(crate) const MAX_LINES: u64 = 5000;
const REPLY_CAP: usize = 1 << 20;
const LINE_SHOW: usize = 8 * 1024;
const LINE_KEEP: usize = 1 << 20;
const CHUNK: usize = 64 * 1024;
const MAX_MATCHES: i64 = 1000;
const MAX_CONTEXT: i64 = 50;
const CANCELLED: &str = "stopped early because the command was cancelled";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Enc {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl Enc {
    fn name(self) -> &'static str {
        match self {
            Enc::Utf8 => "utf-8",
            Enc::Utf8Bom => "utf-8 bom",
            Enc::Utf16Le => "utf-16le",
            Enc::Utf16Be => "utf-16be",
        }
    }

    fn bom_len(self) -> u64 {
        match self {
            Enc::Utf8 => 0,
            Enc::Utf8Bom => 3,
            Enc::Utf16Le | Enc::Utf16Be => 2,
        }
    }

    fn unit(self) -> usize {
        match self {
            Enc::Utf16Le | Enc::Utf16Be => 2,
            Enc::Utf8 | Enc::Utf8Bom => 1,
        }
    }

    fn is_char(self, u: &[u8], c: u8) -> bool {
        match self {
            Enc::Utf16Le => u[0] == c && u[1] == 0,
            Enc::Utf16Be => u[0] == 0 && u[1] == c,
            Enc::Utf8 | Enc::Utf8Bom => u[0] == c,
        }
    }

    fn find_newline(self, s: &[u8]) -> Option<usize> {
        if self.unit() == 1 {
            return s.iter().position(|b| *b == b'\n');
        }
        s.chunks_exact(2)
            .position(|u| self.is_char(u, b'\n'))
            .map(|i| i * 2)
    }

    fn decode(self, b: &[u8]) -> String {
        match self {
            Enc::Utf8 | Enc::Utf8Bom => go_lossy(b),
            Enc::Utf16Le | Enc::Utf16Be => {
                let le = self == Enc::Utf16Le;
                let units = b.chunks_exact(2).map(|u| {
                    if le {
                        u16::from_le_bytes([u[0], u[1]])
                    } else {
                        u16::from_be_bytes([u[0], u[1]])
                    }
                });
                let mut s: String = char::decode_utf16(units)
                    .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                    .collect();
                if b.len() % 2 == 1 {
                    s.push(char::REPLACEMENT_CHARACTER);
                }
                s
            }
        }
    }
}

fn detect(f: &mut fs::File) -> io::Result<Enc> {
    let mut head = [0u8; 3];
    let mut n = 0;
    while n < head.len() {
        match f.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    f.seek(SeekFrom::Start(0))?;
    Ok(match head[..n] {
        [0xEF, 0xBB, 0xBF] => Enc::Utf8Bom,
        [0xFF, 0xFE, ..] => Enc::Utf16Le,
        [0xFE, 0xFF, ..] => Enc::Utf16Be,
        _ => Enc::Utf8,
    })
}

struct LineReader<'a> {
    src: &'a fs::File,
    cancel: &'a CancellationToken,
    enc: Enc,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
    cancelled: bool,
}

impl<'a> LineReader<'a> {
    fn new(src: &'a fs::File, enc: Enc, cancel: &'a CancellationToken) -> Self {
        LineReader {
            src,
            cancel,
            enc,
            buf: vec![0u8; CHUNK],
            pos: 0,
            len: 0,
            eof: false,
            cancelled: false,
        }
    }

    fn fill(&mut self) -> io::Result<()> {
        if self.cancel.is_cancelled() {
            self.cancelled = true;
            self.eof = true;
            return Ok(());
        }
        if self.pos > 0 {
            self.buf.copy_within(self.pos..self.len, 0);
            self.len -= self.pos;
            self.pos = 0;
        }
        let mut src = self.src;
        loop {
            match src.read(&mut self.buf[self.len..]) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(());
                }
                Ok(n) => {
                    self.len += n;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn next_line(&mut self, out: &mut Vec<u8>, keep: usize) -> io::Result<Option<bool>> {
        out.clear();
        let enc = self.enc;
        let unit = enc.unit();
        let mut total = 0usize;
        let mut last_cr = false;
        let mut seen = false;
        loop {
            let avail = self.len - self.pos;
            if avail < unit {
                if !self.eof {
                    self.fill()?;
                    continue;
                }
                if self.cancelled {
                    return Ok(None);
                }
                if avail > 0 {
                    if out.len() < keep {
                        out.push(self.buf[self.pos]);
                    }
                    self.pos = self.len;
                    total += 1;
                    last_cr = false;
                    seen = true;
                }
                if !seen {
                    return Ok(None);
                }
                return Ok(Some(finish(out, total, last_cr, unit, keep)));
            }
            let usable = avail - avail % unit;
            let chunk = &self.buf[self.pos..self.pos + usable];
            let nl = enc.find_newline(chunk);
            let take = nl.unwrap_or(usable);
            if take > 0 {
                let room = keep.saturating_sub(out.len());
                out.extend_from_slice(&chunk[..take.min(room)]);
                total += take;
                last_cr = enc.is_char(&chunk[take - unit..take], b'\r');
                seen = true;
            }
            match nl {
                Some(i) => {
                    self.pos += i + unit;
                    return Ok(Some(finish(out, total, last_cr, unit, keep)));
                }
                None => {
                    self.pos += usable;
                    if !self.eof {
                        self.fill()?;
                    }
                }
            }
        }
    }
}

fn finish(out: &mut Vec<u8>, total: usize, last_cr: bool, unit: usize, keep: usize) -> bool {
    let len = if last_cr { total - unit } else { total };
    out.truncate(len);
    len > keep
}

fn show(text: &str, at: usize, cut: bool) -> (String, bool) {
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

fn json_len(s: &str) -> usize {
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

fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn cut_note(n: u64) -> String {
    let kib = LINE_SHOW / 1024;
    if n == 1 {
        format!("1 long line was cut to {kib} KiB")
    } else {
        format!("{n} long lines were cut to {kib} KiB")
    }
}

fn line_count_arg(args: &Args) -> u64 {
    let n = int_arg(args, "lines").unwrap_or(80);
    if n <= 0 {
        80
    } else {
        (n as u64).min(MAX_LINES)
    }
}

struct Opened {
    kind: &'static str,
    path: String,
    size: u64,
    mtime: String,
    file: fs::File,
    enc: Enc,
}

fn open(kind: &'static str, args: &Args) -> Result<Opened, HandlerError> {
    let path = local_path(kind, &str_arg(args, "path").unwrap_or_default())?;
    guard_read_path(kind, &path)?;
    let info = stat(&path).map_err(|e| read_err(kind, "GetFileAttributesEx", &path, &e))?;
    if info.dir {
        return Err(err(format!("{kind}: {path} is a folder, not a file")));
    }
    let mut file = fs::File::open(&path).map_err(|e| read_err(kind, "open", &path, &e))?;
    let enc = detect(&mut file).map_err(|e| read_err(kind, "read", &path, &e))?;
    Ok(Opened {
        kind,
        path,
        size: info.size,
        mtime: fmt_time(info.mtime),
        file,
        enc,
    })
}

impl Opened {
    fn io(&self, op: &'static str) -> impl Fn(io::Error) -> HandlerError + '_ {
        move |e| read_err(self.kind, op, &self.path, &e)
    }

    fn seek(&mut self, to: u64) -> Result<(), HandlerError> {
        let kind = self.kind;
        let path = &self.path;
        self.file
            .seek(SeekFrom::Start(to))
            .map(|_| ())
            .map_err(|e| read_err(kind, "seek", path, &e))
    }

    fn base(&self, mode: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("path".into(), json!(self.path));
        m.insert("size".into(), json!(self.size));
        m.insert("mtime".into(), json!(self.mtime));
        m.insert("encoding".into(), json!(self.enc.name()));
        m.insert("mode".into(), json!(mode));
        m
    }
}

pub(crate) fn has_range(args: &Args) -> bool {
    args.contains_key("start_line") || args.contains_key("end_line")
}

pub(crate) fn range(args: Args, cancel: &CancellationToken) -> CapResult {
    let start = int_arg(&args, "start_line").unwrap_or(1).max(1) as u64;
    let asked_end = match int_arg(&args, "end_line") {
        Some(e) => {
            let e = e.max(1) as u64;
            if e < start {
                return Err(err("fs.read: start_line is after end_line"));
            }
            e
        }
        None => start + MAX_LINES - 1,
    };
    let end = asked_end.min(start + MAX_LINES - 1);
    let o = open("fs.read", &args)?;
    forward(o, "range", start, end, asked_end > end, cancel)
}

pub(crate) fn head(args: Args, cancel: &CancellationToken) -> CapResult {
    let lines = line_count_arg(&args);
    let o = open("fs.head", &args)?;
    forward(o, "head", 1, lines, false, cancel)
}

fn forward(
    mut o: Opened,
    mode: &str,
    start: u64,
    end: u64,
    span_cut: bool,
    cancel: &CancellationToken,
) -> CapResult {
    let enc = o.enc;
    o.seek(enc.bom_len())?;
    let keep = LINE_SHOW * enc.unit() + 4;
    let mut rd = LineReader::new(&o.file, enc, cancel);
    let mut raw = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut n = 0u64;
    let mut bytes = 0usize;
    let mut cut = 0u64;
    let mut full = false;
    let mut eof = false;
    loop {
        let k = if n + 1 < start { 0 } else { keep };
        let Some(c) = rd.next_line(&mut raw, k).map_err(o.io("read"))? else {
            eof = !rd.cancelled;
            break;
        };
        n += 1;
        if n < start {
            continue;
        }
        let (s, was_cut) = show(&enc.decode(&raw), 0, c);
        let size = json_len(&s);
        if !lines.is_empty() && bytes + size > REPLY_CAP {
            full = true;
            break;
        }
        bytes += size;
        cut += u64::from(was_cut);
        lines.push(s);
        if n >= end {
            break;
        }
    }
    if !eof && !full && !rd.cancelled {
        eof = rd.next_line(&mut raw, 0).map_err(o.io("read"))?.is_none() && !rd.cancelled;
    }

    let count = lines.len() as u64;
    let mut notes: Vec<String> = Vec::new();
    let mut truncated = false;
    if rd.cancelled {
        truncated = true;
        notes.push(CANCELLED.to_string());
    }
    if full {
        truncated = true;
        notes.push(format!(
            "stopped at line {} to keep the reply under 1 MiB; read on from line {}",
            start + count - 1,
            start + count
        ));
    } else if span_cut && !eof {
        truncated = true;
        notes.push(format!(
            "only {MAX_LINES} lines fit in one reply; read on from line {}",
            end + 1
        ));
    }
    if cut > 0 {
        truncated = true;
        notes.push(cut_note(cut));
    }
    if eof && n == 0 {
        notes.push("the file is empty".to_string());
    } else if eof && n < start {
        notes.push(format!("the file has only {}", plural(n, "line")));
    }

    let mut m = o.base(mode);
    let (first, last) = if count > 0 {
        (json!(start), json!(start + count - 1))
    } else {
        (Value::Null, Value::Null)
    };
    m.insert("first_line".into(), first);
    m.insert("last_line".into(), last);
    m.insert("line_count".into(), json!(count));
    m.insert("lines".into(), json!(lines));
    m.insert("eof".into(), json!(eof));
    if eof {
        m.insert("total_lines".into(), json!(n));
    }
    m.insert("truncated".into(), json!(truncated));
    if !notes.is_empty() {
        m.insert("note".into(), json!(notes.join("; ")));
    }
    Ok(Value::Object(m))
}

fn tail_start(
    f: &mut fs::File,
    enc: Enc,
    size: u64,
    want: u64,
    cancel: &CancellationToken,
) -> io::Result<Option<u64>> {
    let unit = enc.unit() as u64;
    let data = enc.bom_len().min(size);
    let end = data + (size - data) / unit * unit;
    let mut pos = end;
    let mut found = 0u64;
    let mut buf = vec![0u8; CHUNK];
    while pos > data {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let from = pos.saturating_sub(CHUNK as u64).max(data);
        let len = (pos - from) as usize;
        f.seek(SeekFrom::Start(from))?;
        f.read_exact(&mut buf[..len])?;
        let mut i = len;
        while i >= unit as usize {
            i -= unit as usize;
            let at = from + i as u64;
            let last = at + unit == end && end == size;
            if !last && enc.is_char(&buf[i..i + unit as usize], b'\n') {
                found += 1;
                if found == want {
                    return Ok(Some(at + unit));
                }
            }
        }
        pos = from;
    }
    Ok(Some(data))
}

pub(crate) fn tail(args: Args, cancel: &CancellationToken) -> CapResult {
    let want = line_count_arg(&args);
    let mut o = open("fs.tail", &args)?;
    let enc = o.enc;
    let size = o.size;
    let from = tail_start(&mut o.file, enc, size, want, cancel).map_err(o.io("read"))?;
    let mut q: VecDeque<(String, bool, usize)> = VecDeque::new();
    let mut bytes = 0usize;
    let mut squeezed = false;
    let mut cancelled = from.is_none();
    if let Some(from) = from {
        o.seek(from)?;
        let keep = LINE_SHOW * enc.unit() + 4;
        let mut rd = LineReader::new(&o.file, enc, cancel);
        let mut raw = Vec::new();
        while let Some(c) = rd.next_line(&mut raw, keep).map_err(o.io("read"))? {
            let (s, was_cut) = show(&enc.decode(&raw), 0, c);
            let size = json_len(&s);
            bytes += size;
            q.push_back((s, was_cut, size));
            while q.len() as u64 > want {
                if let Some((_, _, sz)) = q.pop_front() {
                    bytes -= sz;
                }
            }
            while bytes > REPLY_CAP && q.len() > 1 {
                if let Some((_, _, sz)) = q.pop_front() {
                    bytes -= sz;
                    squeezed = true;
                }
            }
        }
        cancelled = rd.cancelled;
    }

    let cut = q.iter().filter(|l| l.1).count() as u64;
    let lines: Vec<String> = q.into_iter().map(|l| l.0).collect();
    let mut notes: Vec<String> = Vec::new();
    if cancelled {
        notes.push(CANCELLED.to_string());
    }
    if squeezed {
        notes.push(format!(
            "only the last {} fit in 1 MiB",
            plural(lines.len() as u64, "line")
        ));
    }
    if cut > 0 {
        notes.push(cut_note(cut));
    }
    let mut m = o.base("tail");
    m.insert("line_count".into(), json!(lines.len()));
    m.insert("lines".into(), json!(lines));
    m.insert("truncated".into(), json!(!notes.is_empty()));
    if !notes.is_empty() {
        m.insert("note".into(), json!(notes.join("; ")));
    }
    Ok(Value::Object(m))
}

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

pub(crate) fn grep(args: Args, cancel: &CancellationToken) -> CapResult {
    let mut o = open("fs.grep", &args)?;
    let text = str_arg(&args, "text").unwrap_or_default();
    if text.is_empty() {
        return Err(err("fs.grep: 'text' is required"));
    }
    let case_sensitive = bool_arg(&args, "case_sensitive").unwrap_or(false);
    let before = int_arg(&args, "before").unwrap_or(0).clamp(0, MAX_CONTEXT) as usize;
    let after = int_arg(&args, "after").unwrap_or(0).clamp(0, MAX_CONTEXT) as usize;
    let max_matches = match int_arg(&args, "max_matches").unwrap_or(100) {
        m if m <= 0 => 100,
        m => m.min(MAX_MATCHES) as u64,
    };
    let start = int_arg(&args, "start_line").unwrap_or(1).max(1) as u64;
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
        let k = if n + 1 + (before as u64) < start {
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
                if match_lines.len() as u64 >= max_matches {
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
                if out.len() + add.len() > MAX_LINES as usize || bytes + size > REPLY_CAP {
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
                    if out.len() + 1 > MAX_LINES as usize || bytes + size > REPLY_CAP {
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

    let listed = match_lines.len() as u64;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{Ctx, Outcome, Router};
    use std::io::{BufWriter, Write};
    use std::path::Path;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    async fn run(kind: &str, v: Value) -> Outcome {
        let mut r = Router::new();
        super::super::fs::register(&mut r);
        r.dispatch(Ctx::default(), kind, args(v)).await
    }

    fn lines(o: &Outcome) -> Vec<String> {
        o.result["lines"]
            .as_array()
            .unwrap_or_else(|| panic!("no lines: {} {}", o.error, o.result))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn utf16(text: &str, le: bool) -> Vec<u8> {
        let mut out = if le {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for u in text.encode_utf16() {
            out.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        out
    }

    fn big_line(i: u64) -> String {
        format!("line {i:08} the quick brown fox jumps over the lazy dog 0123456789")
    }

    #[tokio::test]
    async fn a_50_mb_file_reads_in_the_middle_and_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let total = 800_000u64;
        {
            let mut w = BufWriter::new(fs::File::create(&path).unwrap());
            for i in 1..=total {
                writeln!(w, "{}", big_line(i)).unwrap();
            }
        }
        assert!(fs::metadata(&path).unwrap().len() >= 50 * 1024 * 1024);
        let path = path.to_string_lossy().into_owned();

        let mid = run(
            "fs.read",
            json!({"path": path, "start_line": 400_000, "end_line": 400_004}),
        )
        .await;
        assert!(mid.ok, "{}", mid.error);
        let got = lines(&mid);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0], big_line(400_000));
        assert_eq!(got[4], big_line(400_004));
        assert_eq!(mid.result["first_line"], json!(400_000));
        assert_eq!(mid.result["last_line"], json!(400_004));
        assert_eq!(mid.result["eof"], json!(false));
        assert_eq!(mid.result["truncated"], json!(false));
        assert!(mid.result.get("base64").is_none());

        let end = run(
            "fs.read",
            json!({"path": path, "start_line": total - 2, "end_line": total + 100}),
        )
        .await;
        assert!(end.ok, "{}", end.error);
        assert_eq!(
            lines(&end),
            vec![big_line(total - 2), big_line(total - 1), big_line(total)]
        );
        assert_eq!(end.result["eof"], json!(true));
        assert_eq!(end.result["total_lines"], json!(total));
        assert_eq!(end.result["truncated"], json!(false));

        let tail = run("fs.tail", json!({"path": path, "lines": 3})).await;
        assert!(tail.ok, "{}", tail.error);
        assert_eq!(
            lines(&tail),
            vec![big_line(total - 2), big_line(total - 1), big_line(total)]
        );

        let head = run("fs.head", json!({"path": path, "lines": 2})).await;
        assert_eq!(lines(&head), vec![big_line(1), big_line(2)]);
        assert_eq!(head.result["eof"], json!(false));

        let found = run(
            "fs.grep",
            json!({"path": path, "text": "LINE 00799999 ", "before": 1}),
        )
        .await;
        assert!(found.ok, "{}", found.error);
        assert_eq!(found.result["match_count"], json!(1));
        assert_eq!(found.result["match_lines"], json!([799_999]));
        assert_eq!(found.result["lines_scanned"], json!(total));
        assert_eq!(
            lines(&found),
            vec![
                format!("799998- {}", big_line(799_998)),
                format!("799999: {}", big_line(799_999)),
            ]
        );

        let span = run(
            "fs.read",
            json!({"path": path, "start_line": 10, "end_line": 20_000}),
        )
        .await;
        assert!(span.ok, "{}", span.error);
        assert_eq!(span.result["line_count"], json!(5000));
        assert_eq!(span.result["last_line"], json!(5009));
        assert_eq!(span.result["truncated"], json!(true));
        assert_eq!(
            span.result["note"],
            json!("only 5000 lines fit in one reply; read on from line 5010")
        );

        let whole = run("fs.read", json!({"path": path})).await;
        assert!(!whole.ok);
        for want in [
            "over the 8 MiB limit",
            "start_line and end_line",
            "fs.head",
            "fs.tail",
            "fs.grep",
            "or download it.",
        ] {
            assert!(whole.error.contains(want), "{want}: {}", whole.error);
        }
        let dl = super::super::fs::fs_download(args(json!({"path": path}))).unwrap_err();
        assert!(
            dl.message
                .ends_with("Download it in parts with offset and length."),
            "{}",
            dl.message
        );
    }

    #[tokio::test]
    async fn utf16_files_with_a_bom_decode_and_number_lines() {
        let dir = tempfile::tempdir().unwrap();
        let text = "first\r\nsecond \u{fc}ber\r\nthird\r\n";
        for (name, le, enc) in [("le.txt", true, "utf-16le"), ("be.txt", false, "utf-16be")] {
            let path = write(dir.path(), name, &utf16(text, le));
            let head = run("fs.head", json!({"path": path})).await;
            assert!(head.ok, "{}", head.error);
            assert_eq!(lines(&head), vec!["first", "second \u{fc}ber", "third"]);
            assert_eq!(head.result["encoding"], json!(enc));
            assert_eq!(head.result["total_lines"], json!(3));

            let mid = run(
                "fs.read",
                json!({"path": path, "start_line": 2, "end_line": 2}),
            )
            .await;
            assert_eq!(lines(&mid), vec!["second \u{fc}ber"]);

            let tail = run("fs.tail", json!({"path": path, "lines": 2})).await;
            assert_eq!(lines(&tail), vec!["second \u{fc}ber", "third"]);

            let hit = run("fs.grep", json!({"path": path, "text": "\u{dc}BER"})).await;
            assert!(hit.ok, "{}", hit.error);
            assert_eq!(hit.result["match_lines"], json!([2]));
            assert_eq!(lines(&hit), vec!["2: second \u{fc}ber"]);
        }
    }

    #[tokio::test]
    async fn utf8_bom_and_bad_bytes_decode_lossily() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "bom.txt",
            b"\xEF\xBB\xBFone\nbad \xFF byte\nthree",
        );
        let out = run("fs.head", json!({"path": path})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(lines(&out), vec!["one", "bad \u{FFFD} byte", "three"]);
        assert_eq!(out.result["encoding"], json!("utf-8 bom"));
        let tail = run("fs.tail", json!({"path": path, "lines": 5})).await;
        assert_eq!(lines(&tail), vec!["one", "bad \u{FFFD} byte", "three"]);
    }

    #[tokio::test]
    async fn crlf_and_lf_files_number_lines_the_same() {
        let dir = tempfile::tempdir().unwrap();
        for (name, nl) in [("crlf.txt", "\r\n"), ("lf.txt", "\n")] {
            let body = ["a", "b", "", "needle", "c"].join(nl) + nl;
            let path = write(dir.path(), name, body.as_bytes());
            let hit = run(
                "fs.grep",
                json!({"path": path, "text": "needle", "after": 1}),
            )
            .await;
            assert!(hit.ok, "{}", hit.error);
            assert_eq!(hit.result["match_lines"], json!([4]));
            assert_eq!(lines(&hit), vec!["4: needle", "5- c"]);
            let one = run(
                "fs.read",
                json!({"path": path, "start_line": 3, "end_line": 5}),
            )
            .await;
            assert_eq!(lines(&one), vec!["", "needle", "c"]);
            assert_eq!(one.result["total_lines"], json!(5));
            let tail = run("fs.tail", json!({"path": path, "lines": 2})).await;
            assert_eq!(lines(&tail), vec!["needle", "c"]);
        }
    }

    #[tokio::test]
    async fn ranges_past_the_end_and_backwards_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "three.txt", b"x\ny\nz\n");
        let past = run(
            "fs.read",
            json!({"path": path, "start_line": 10, "end_line": 20}),
        )
        .await;
        assert!(past.ok, "{}", past.error);
        assert_eq!(past.result["line_count"], json!(0));
        assert_eq!(past.result["first_line"], Value::Null);
        assert_eq!(past.result["eof"], json!(true));
        assert_eq!(past.result["note"], json!("the file has only 3 lines"));

        let only_end = run("fs.read", json!({"path": path, "end_line": 2})).await;
        assert_eq!(lines(&only_end), vec!["x", "y"]);
        assert_eq!(only_end.result["eof"], json!(false));

        let back = run(
            "fs.read",
            json!({"path": path, "start_line": 3, "end_line": 2}),
        )
        .await;
        assert!(!back.ok);
        assert_eq!(back.error, "fs.read: start_line is after end_line");

        let empty = write(dir.path(), "empty.txt", b"");
        for kind in ["fs.head", "fs.tail"] {
            let out = run(kind, json!({"path": empty})).await;
            assert!(out.ok, "{kind}: {}", out.error);
            assert_eq!(out.result["line_count"], json!(0), "{kind}");
        }
        let out = run("fs.head", json!({"path": empty})).await;
        assert_eq!(out.result["note"], json!("the file is empty"));
    }

    #[tokio::test]
    async fn replies_stay_under_the_size_cap_and_long_lines_are_cut() {
        let dir = tempfile::tempdir().unwrap();
        let row = "w".repeat(3000);
        let mut body = String::new();
        for _ in 0..1000 {
            body.push_str(&row);
            body.push('\n');
        }
        let path = write(dir.path(), "wide.txt", body.as_bytes());
        let out = run("fs.head", json!({"path": path, "lines": 1000})).await;
        assert!(out.ok, "{}", out.error);
        let count = out.result["line_count"].as_u64().unwrap();
        assert!(count > 300 && count < 1000, "{count}");
        assert_eq!(out.result["truncated"], json!(true));
        let note = out.result["note"].as_str().unwrap();
        assert!(
            note.starts_with(&format!("stopped at line {count} ")),
            "{note}"
        );
        assert!(
            note.ends_with(&format!("read on from line {}", count + 1)),
            "{note}"
        );
        assert!(serde_json::to_vec(&out.result).unwrap().len() < REPLY_CAP + 4096);

        let tail = run("fs.tail", json!({"path": path, "lines": 1000})).await;
        let tcount = tail.result["line_count"].as_u64().unwrap();
        assert!(tcount > 300 && tcount < 1000, "{tcount}");
        assert_eq!(
            tail.result["note"],
            json!(format!("only the last {tcount} lines fit in 1 MiB"))
        );

        let long = format!(
            "start {} NEEDLE {}\nshort\n",
            "a".repeat(20_000),
            "b".repeat(100)
        );
        let path = write(dir.path(), "long.txt", long.as_bytes());
        let head = run("fs.head", json!({"path": path})).await;
        let got = lines(&head);
        assert!(got[0].starts_with("start aaa") && got[0].ends_with("..."));
        assert!(got[0].len() <= LINE_SHOW + 3);
        assert_eq!(got[1], "short");
        assert_eq!(head.result["note"], json!("1 long line was cut to 8 KiB"));

        let hit = run("fs.grep", json!({"path": path, "text": "needle"})).await;
        let got = lines(&hit);
        assert!(
            got[0].starts_with("1: ...") && got[0].contains("NEEDLE bbb"),
            "{}",
            got[0]
        );
        assert_eq!(hit.result["truncated"], json!(true));
    }

    #[tokio::test]
    async fn match_cap_still_counts_every_match() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=50).map(|i| format!("hit {i}\nmiss\n")).collect();
        let path = write(dir.path(), "hits.txt", body.as_bytes());
        let out = run(
            "fs.grep",
            json!({"path": path, "text": "hit", "max_matches": 10}),
        )
        .await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["listed"], json!(10));
        assert_eq!(out.result["match_count"], json!(50));
        assert_eq!(out.result["truncated"], json!(true));
        assert_eq!(
            out.result["note"],
            json!("listed 10 of 50 matches; search again with start_line 20 for more")
        );
        let next = run(
            "fs.grep",
            json!({"path": path, "text": "hit", "max_matches": 10, "start_line": 20}),
        )
        .await;
        assert_eq!(next.result["match_lines"][0], json!(21));
        assert_eq!(next.result["match_count"], json!(40));
    }

    #[tokio::test]
    async fn overlapping_context_is_merged() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=20)
            .map(|i| {
                if i == 5 || i == 7 || i == 15 {
                    format!("hit {i}\n")
                } else {
                    format!("x{i}\n")
                }
            })
            .collect();
        let path = write(dir.path(), "ctx.txt", body.as_bytes());
        let out = run(
            "fs.grep",
            json!({"path": path, "text": "hit", "before": 2, "after": 2}),
        )
        .await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(
            lines(&out),
            vec![
                "3- x3",
                "4- x4",
                "5: hit 5",
                "6- x6",
                "7: hit 7",
                "8- x8",
                "9- x9",
                "--",
                "13- x13",
                "14- x14",
                "15: hit 15",
                "16- x16",
                "17- x17",
            ]
        );
        assert_eq!(out.result["match_lines"], json!([5, 7, 15]));

        let plain = run("fs.grep", json!({"path": path, "text": "hit"})).await;
        assert_eq!(lines(&plain), vec!["5: hit 5", "7: hit 7", "15: hit 15"]);
    }

    #[tokio::test]
    async fn case_matching_can_be_turned_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "case.txt",
            b"Error here\nerror there\nERROR\nfine\n",
        );
        let loose = run("fs.grep", json!({"path": path, "text": "error"})).await;
        assert_eq!(loose.result["match_lines"], json!([1, 2, 3]));
        assert_eq!(loose.result["case_sensitive"], json!(false));
        let strict = run(
            "fs.grep",
            json!({"path": path, "text": "error", "case_sensitive": true}),
        )
        .await;
        assert_eq!(strict.result["match_lines"], json!([2]));
        assert_eq!(strict.result["match_count"], json!(1));
    }

    #[test]
    fn cancelling_stops_every_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "c.txt", b"one\ntwo\nthree\n");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let g = grep(args(json!({"path": path, "text": "o"})), &cancel).unwrap();
        assert_eq!(g["lines_scanned"], json!(0));
        assert_eq!(g["truncated"], json!(true));
        assert_eq!(g["note"], json!(CANCELLED));
        let r = range(args(json!({"path": path, "start_line": 1})), &cancel).unwrap();
        assert_eq!(r["truncated"], json!(true));
        assert_eq!(r["eof"], json!(false));
        assert_eq!(r["note"], json!(CANCELLED));
        let t = tail(args(json!({"path": path})), &cancel).unwrap();
        assert_eq!(t["line_count"], json!(0));
        assert_eq!(t["note"], json!(CANCELLED));
    }

    #[tokio::test]
    async fn secret_paths_and_missing_files_are_refused_plainly() {
        let secret = "C:\\Users\\x\\.ssh\\id_rsa";
        for (kind, extra) in [
            ("fs.grep", json!({"text": "a"})),
            ("fs.read", json!({"start_line": 1})),
            ("fs.head", json!({})),
            ("fs.tail", json!({})),
        ] {
            let mut a = extra.as_object().cloned().unwrap();
            a.insert("path".into(), json!(secret));
            let out = run(kind, Value::Object(a)).await;
            assert!(!out.ok, "{kind}");
            assert!(
                out.error.contains("can hold passwords or keys"),
                "{kind}: {}",
                out.error
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.txt").to_string_lossy().into_owned();
        for kind in ["fs.read", "fs.head", "fs.tail", "fs.grep", "fs.stat"] {
            let out = run(kind, json!({"path": missing, "text": "a"})).await;
            assert_eq!(out.error, format!("{kind}: {missing} doesn't exist"));
        }
        let folder = dir.path().to_string_lossy().into_owned();
        for kind in ["fs.read", "fs.head", "fs.grep"] {
            let out = run(kind, json!({"path": folder, "text": "a"})).await;
            assert_eq!(
                out.error,
                format!("{kind}: {folder} is a folder, not a file")
            );
        }
        let file = write(dir.path(), "f.txt", b"x");
        let ls = run("fs.ls", json!({"path": file})).await;
        assert_eq!(ls.error, format!("fs.ls: {file} is a file, not a folder"));
    }

    #[tokio::test]
    async fn grep_needs_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "t.txt", b"x\n");
        let out = run("fs.grep", json!({"path": path, "text": ""})).await;
        assert!(!out.ok);
        assert!(out.error.contains("'text'"), "{}", out.error);
    }
}

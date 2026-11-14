use super::fs::bool_arg;
use super::fs::err;
use super::fs::int_arg;
use super::fs::str_arg;
use super::fs_lines::CANCELLED;
use super::fs_lines::MAX_LINES;
use super::fs_lines::REPLY_CAP;
use super::fs_lines_reader::LineReader;
use super::fs_lines_reader::Opened;
use super::fs_lines_reader::open;
use super::fs_lines_reply::cut_note;
use super::fs_lines_reply::json_len;
use super::fs_lines_reply::show;
use crate::dispatch::{Args, CapResult, HandlerError};
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
                        .map_or(line.len(), |(b, _)| b),
                )
            }
        }
    }
}

struct Query {
    text: String,
    case_sensitive: bool,
    before: usize,
    after: usize,
    max_matches: u64,
    start: u64,
}

impl Query {
    fn from_args(args: &Args) -> Result<Query, HandlerError> {
        let text = str_arg(args, "text").unwrap_or_default();
        if text.is_empty() {
            return Err(err("fs.grep: 'text' is required"));
        }
        let max_matches = match int_arg(args, "max_matches").unwrap_or(100) {
            limit if limit <= 0 => 100,
            limit => crate::util::positive_u64(limit.min(MAX_MATCHES)),
        };
        Ok(Query {
            text,
            case_sensitive: bool_arg(args, "case_sensitive").unwrap_or(false),
            before: context_arg(args, "before"),
            after: context_arg(args, "after"),
            max_matches,
            start: crate::util::positive_u64(int_arg(args, "start_line").unwrap_or(1).max(1)),
        })
    }
}

fn context_arg(args: &Args, name: &str) -> usize {
    usize::try_from(int_arg(args, name).unwrap_or(0).clamp(0, MAX_CONTEXT)).unwrap_or(0)
}

struct Listing {
    lines: Vec<String>,
    bytes: usize,
    cut: u64,
    accepting: bool,
    capped: bool,
    last_line: u64,
}

impl Listing {
    fn new() -> Listing {
        Listing {
            lines: Vec::new(),
            bytes: 0,
            cut: 0,
            accepting: true,
            capped: false,
            last_line: 0,
        }
    }

    fn stop(&mut self) {
        self.accepting = false;
        self.capped = true;
    }

    fn add(&mut self, add: Vec<String>, cut: u64, line_no: u64) -> bool {
        let size: usize = add.iter().map(|entry| json_len(entry)).sum();
        if self.lines.len() + add.len() > crate::util::size_usize(MAX_LINES)
            || self.bytes + size > REPLY_CAP
        {
            self.stop();
            return false;
        }
        self.lines.extend(add);
        self.bytes += size;
        self.cut += cut;
        self.last_line = line_no;
        true
    }
}

struct Scan {
    listing: Listing,
    match_lines: Vec<u64>,
    total: u64,
    long: u64,
    scanned: u64,
    cancelled: bool,
}

fn with_context(
    ring: &mut VecDeque<(u64, String, bool)>,
    last_line: u64,
    context: bool,
    line_no: u64,
    (shown, was_cut): (String, bool),
) -> (Vec<String>, u64) {
    let mut prev = last_line;
    let mut add: Vec<String> = Vec::new();
    let mut add_cut = 0u64;
    for (number, text, cut) in ring.drain(..) {
        if number <= prev {
            continue;
        }
        if context && prev > 0 && number > prev + 1 {
            add.push("--".to_string());
        }
        add.push(format!("{number}- {text}"));
        add_cut += u64::from(cut);
        prev = number;
    }
    if context && prev > 0 && line_no > prev + 1 {
        add.push("--".to_string());
    }
    add.push(format!("{line_no}: {shown}"));
    add_cut += u64::from(was_cut);
    (add, add_cut)
}

fn scan(opened: &Opened, query: &Query, cancel: &CancellationToken) -> Result<Scan, HandlerError> {
    let enc = opened.enc;
    let matcher = Matcher::new(&query.text, query.case_sensitive);
    let context = query.before > 0 || query.after > 0;
    let mut reader = LineReader::new(&opened.file, enc, cancel);
    let mut raw = Vec::new();
    let mut ring: VecDeque<(u64, String, bool)> = VecDeque::with_capacity(query.before + 1);
    let mut listing = Listing::new();
    let mut match_lines: Vec<u64> = Vec::new();
    let (mut total, mut long, mut line_no, mut after_left) = (0u64, 0u64, 0u64, 0usize);
    loop {
        let keep = if line_no + 1 + crate::util::count_u64(query.before) < query.start {
            0
        } else {
            LINE_KEEP
        };
        let Some(over) = reader
            .next_line(&mut raw, keep)
            .map_err(opened.io("read"))?
        else {
            break;
        };
        line_no += 1;
        if keep == 0 {
            continue;
        }
        let line = enc.decode(&raw);
        let hit = if line_no >= query.start {
            if over {
                long += 1;
            }
            matcher.find(&line)
        } else {
            None
        };
        if let Some(at) = hit {
            total += 1;
            if !listing.accepting {
                continue;
            }
            if crate::util::count_u64(match_lines.len()) >= query.max_matches {
                listing.stop();
                continue;
            }
            let shown = show(&line, at, over);
            let (add, add_cut) =
                with_context(&mut ring, listing.last_line, context, line_no, shown);
            if listing.add(add, add_cut, line_no) {
                match_lines.push(line_no);
                after_left = query.after;
            }
        } else if listing.accepting {
            if after_left > 0 && line_no >= query.start {
                after_left -= 1;
                let (shown, was_cut) = show(&line, 0, over);
                listing.add(
                    vec![format!("{line_no}- {shown}")],
                    u64::from(was_cut),
                    line_no,
                );
            } else if query.before > 0 {
                let (shown, was_cut) = show(&line, 0, over);
                ring.push_back((line_no, shown, was_cut));
                if ring.len() > query.before {
                    ring.pop_front();
                }
            }
        }
    }
    Ok(Scan {
        listing,
        match_lines,
        total,
        long,
        scanned: line_no,
        cancelled: reader.cancelled,
    })
}

fn notes(scan: &Scan, query: &Query, listed: u64) -> Vec<String> {
    let mut notes: Vec<String> = Vec::new();
    if scan.cancelled {
        notes.push(CANCELLED.to_string());
    }
    if scan.listing.capped {
        let resume = scan.match_lines.last().map_or(query.start, |line| line + 1);
        notes.push(format!(
            "listed {listed} of {} matches; search again with start_line {resume} for more",
            scan.total
        ));
    }
    if scan.listing.cut > 0 {
        notes.push(cut_note(scan.listing.cut));
    }
    if scan.long == 1 {
        notes.push("1 line over 1 MiB was only searched in its first 1 MiB".to_string());
    } else if scan.long > 1 {
        notes.push(format!(
            "{} lines over 1 MiB were only searched in their first 1 MiB",
            scan.long
        ));
    }
    notes
}

pub(crate) fn grep(args: &Args, cancel: &CancellationToken) -> CapResult {
    let mut opened = open("fs.grep", args)?;
    let query = Query::from_args(args)?;
    opened.seek(opened.enc.bom_len())?;
    let scan = scan(&opened, &query, cancel)?;
    let listed = crate::util::count_u64(scan.match_lines.len());
    let notes = notes(&scan, &query, listed);
    let truncated = scan.cancelled || scan.listing.capped || scan.listing.cut > 0 || scan.long > 0;
    let mut reply = opened.base("grep");
    reply.insert("text".into(), json!(query.text));
    reply.insert("case_sensitive".into(), json!(query.case_sensitive));
    reply.insert("start_line".into(), json!(query.start));
    reply.insert("before".into(), json!(query.before));
    reply.insert("after".into(), json!(query.after));
    reply.insert("max_matches".into(), json!(query.max_matches));
    reply.insert("match_count".into(), json!(scan.total));
    reply.insert("listed".into(), json!(listed));
    reply.insert("match_lines".into(), json!(scan.match_lines));
    reply.insert("lines_scanned".into(), json!(scan.scanned));
    reply.insert("line_count".into(), json!(scan.listing.lines.len()));
    reply.insert("lines".into(), json!(scan.listing.lines));
    reply.insert("truncated".into(), json!(truncated));
    if !notes.is_empty() {
        reply.insert("note".into(), json!(notes.join("; ")));
    }
    Ok(Value::Object(reply))
}

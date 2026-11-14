use super::fs::err;
use super::fs::int_arg;
use super::fs_lines::CANCELLED;
use super::fs_lines::LINE_SHOW;
use super::fs_lines::MAX_LINES;
use super::fs_lines::REPLY_CAP;
use super::fs_lines_reader::LineReader;
use super::fs_lines_reader::Opened;
use super::fs_lines_reader::open;
use super::fs_lines_reply::cut_note;
use super::fs_lines_reply::json_len;
use super::fs_lines_reply::line_count_arg;
use super::fs_lines_reply::plural;
use super::fs_lines_reply::show;
use crate::dispatch::{Args, CapResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub(crate) fn has_range(args: &Args) -> bool {
    args.contains_key("start_line") || args.contains_key("end_line")
}

pub(crate) fn range(args: &Args, cancel: &CancellationToken) -> CapResult {
    let start = crate::util::positive_u64(int_arg(args, "start_line").unwrap_or(1).max(1));
    let asked_end = match int_arg(args, "end_line") {
        Some(e) => {
            let e = crate::util::positive_u64(e.max(1));
            if e < start {
                return Err(err("fs.read: start_line is after end_line"));
            }
            e
        }
        None => start + MAX_LINES - 1,
    };
    let end = asked_end.min(start + MAX_LINES - 1);
    let o = open("fs.read", args)?;
    forward(o, "range", start, end, asked_end > end, cancel)
}

pub(crate) fn head(args: &Args, cancel: &CancellationToken) -> CapResult {
    let lines = line_count_arg(args);
    let o = open("fs.head", args)?;
    forward(o, "head", 1, lines, false, cancel)
}

fn forward(
    mut opened: Opened,
    mode: &str,
    start: u64,
    end: u64,
    span_cut: bool,
    cancel: &CancellationToken,
) -> CapResult {
    let enc = opened.enc;
    opened.seek(enc.bom_len())?;
    let keep = LINE_SHOW * enc.unit() + 4;
    let mut reader = LineReader::new(&opened.file, enc, cancel);
    let mut raw = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut line_no = 0u64;
    let mut bytes = 0usize;
    let mut cut = 0u64;
    let mut full = false;
    let mut eof = false;
    loop {
        let take = if line_no + 1 < start { 0 } else { keep };
        let Some(over) = reader
            .next_line(&mut raw, take)
            .map_err(opened.io("read"))?
        else {
            eof = !reader.cancelled;
            break;
        };
        line_no += 1;
        if line_no < start {
            continue;
        }
        let (shown, was_cut) = show(&enc.decode(&raw), 0, over);
        let size = json_len(&shown);
        if !lines.is_empty() && bytes + size > REPLY_CAP {
            full = true;
            break;
        }
        bytes += size;
        cut += u64::from(was_cut);
        lines.push(shown);
        if line_no >= end {
            break;
        }
    }
    if !eof && !full && !reader.cancelled {
        eof = reader
            .next_line(&mut raw, 0)
            .map_err(opened.io("read"))?
            .is_none()
            && !reader.cancelled;
    }

    let count = crate::util::count_u64(lines.len());
    let mut notes: Vec<String> = Vec::new();
    let mut truncated = false;
    if reader.cancelled {
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
    if eof && line_no == 0 {
        notes.push("the file is empty".to_string());
    } else if eof && line_no < start {
        notes.push(format!("the file has only {}", plural(line_no, "line")));
    }

    let mut reply = opened.base(mode);
    let (first, last) = if count > 0 {
        (json!(start), json!(start + count - 1))
    } else {
        (Value::Null, Value::Null)
    };
    reply.insert("first_line".into(), first);
    reply.insert("last_line".into(), last);
    reply.insert("line_count".into(), json!(count));
    reply.insert("lines".into(), json!(lines));
    reply.insert("eof".into(), json!(eof));
    if eof {
        reply.insert("total_lines".into(), json!(line_no));
    }
    reply.insert("truncated".into(), json!(truncated));
    if !notes.is_empty() {
        reply.insert("note".into(), json!(notes.join("; ")));
    }
    Ok(Value::Object(reply))
}

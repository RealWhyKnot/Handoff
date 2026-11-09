use super::fs_lines::CANCELLED;
use super::fs_lines::CHUNK;
use super::fs_lines::LINE_SHOW;
use super::fs_lines::REPLY_CAP;
use super::fs_lines_reader::Enc;
use super::fs_lines_reader::LineReader;
use super::fs_lines_reader::open;
use super::fs_lines_reply::cut_note;
use super::fs_lines_reply::json_len;
use super::fs_lines_reply::line_count_arg;
use super::fs_lines_reply::plural;
use super::fs_lines_reply::show;
use crate::dispatch::{Args, CapResult};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use tokio_util::sync::CancellationToken;

pub(super) fn tail_start(
    f: &mut fs::File,
    enc: Enc,
    size: u64,
    want: u64,
    cancel: &CancellationToken,
) -> io::Result<Option<u64>> {
    let unit_len = enc.unit();
    let unit = crate::util::count_u64(unit_len);
    let data = enc.bom_len().min(size);
    let end = data + (size - data) / unit * unit;
    let mut pos = end;
    let mut found = 0u64;
    let mut buf = vec![0u8; CHUNK];
    while pos > data {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let from = pos.saturating_sub(crate::util::count_u64(CHUNK)).max(data);
        let len = crate::util::size_usize(pos - from);
        f.seek(SeekFrom::Start(from))?;
        f.read_exact(&mut buf[..len])?;
        let mut i = len;
        while i >= unit_len {
            i -= unit_len;
            let at = from + crate::util::count_u64(i);
            let last = at + unit == end && end == size;
            if !last && enc.is_char(&buf[i..i + unit_len], b'\n') {
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

pub(crate) fn tail(args: &Args, cancel: &CancellationToken) -> CapResult {
    let want = line_count_arg(args);
    let mut o = open("fs.tail", args)?;
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
            while crate::util::count_u64(q.len()) > want {
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

    let cut = crate::util::count_u64(q.iter().filter(|l| l.1).count());
    let lines: Vec<String> = q.into_iter().map(|l| l.0).collect();
    let mut notes: Vec<String> = Vec::new();
    if cancelled {
        notes.push(CANCELLED.to_string());
    }
    if squeezed {
        notes.push(format!(
            "only the last {} fit in 1 MiB",
            plural(crate::util::count_u64(lines.len()), "line")
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

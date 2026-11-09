use super::fs::err;
use super::fs::read_err;
use super::fs::str_arg;
use super::fs_guard::guard_read_path;
use super::fs_lines::CHUNK;
use super::fs_meta::fmt_time;
use super::fs_meta::stat;
use super::fs_path::local_path;
use super::fs_read::go_lossy;
use crate::dispatch::{Args, HandlerError};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Enc {
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

    pub(super) fn bom_len(self) -> u64 {
        match self {
            Enc::Utf8 => 0,
            Enc::Utf8Bom => 3,
            Enc::Utf16Le | Enc::Utf16Be => 2,
        }
    }

    pub(super) fn unit(self) -> usize {
        match self {
            Enc::Utf16Le | Enc::Utf16Be => 2,
            Enc::Utf8 | Enc::Utf8Bom => 1,
        }
    }

    pub(super) fn is_char(self, u: &[u8], c: u8) -> bool {
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
        s.as_chunks::<2>()
            .0
            .iter()
            .position(|u| self.is_char(u, b'\n'))
            .map(|i| i * 2)
    }

    pub(super) fn decode(self, b: &[u8]) -> String {
        match self {
            Enc::Utf8 | Enc::Utf8Bom => go_lossy(b),
            Enc::Utf16Le | Enc::Utf16Be => {
                let le = self == Enc::Utf16Le;
                let units = b.as_chunks::<2>().0.iter().map(|u| {
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

pub(super) struct LineReader<'a> {
    src: &'a fs::File,
    cancel: &'a CancellationToken,
    enc: Enc,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
    pub(super) cancelled: bool,
}

impl<'a> LineReader<'a> {
    pub(super) fn new(src: &'a fs::File, enc: Enc, cancel: &'a CancellationToken) -> Self {
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

    pub(super) fn next_line(&mut self, out: &mut Vec<u8>, keep: usize) -> io::Result<Option<bool>> {
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

pub(super) struct Opened {
    kind: &'static str,
    path: String,
    pub(super) size: u64,
    mtime: String,
    pub(super) file: fs::File,
    pub(super) enc: Enc,
}

pub(super) fn open(kind: &'static str, args: &Args) -> Result<Opened, HandlerError> {
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
    pub(super) fn io(&self, op: &'static str) -> impl Fn(io::Error) -> HandlerError + '_ {
        move |e| read_err(self.kind, op, &self.path, &e)
    }

    pub(super) fn seek(&mut self, to: u64) -> Result<(), HandlerError> {
        let kind = self.kind;
        let path = &self.path;
        self.file
            .seek(SeekFrom::Start(to))
            .map(|_| ())
            .map_err(|e| read_err(kind, "seek", path, &e))
    }

    pub(super) fn base(&self, mode: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("path".into(), json!(self.path));
        m.insert("size".into(), json!(self.size));
        m.insert("mtime".into(), json!(self.mtime));
        m.insert("encoding".into(), json!(self.enc.name()));
        m.insert("mode".into(), json!(mode));
        m
    }
}

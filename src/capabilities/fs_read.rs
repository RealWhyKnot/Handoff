use super::fs::err;
use super::fs::hex;
use super::fs::int_arg;
use super::fs::path_err;
use super::fs::read_err;
use super::fs::str_arg;
use super::fs_guard::guard_read_path;
use super::fs_meta::fmt_time;
use super::fs_meta::stat;
use super::fs_path::local_path;
use crate::dispatch::{Args, CapResult, HandlerError};
use base64::Engine;
use num_traits::ToPrimitive;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::time::UNIX_EPOCH;

const READ_CAP: u64 = 8 * 1024 * 1024;

pub(super) const CHUNK_CAP: u64 = 4 * 1024 * 1024;

pub(crate) fn go_lossy(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or(""));
                let bad = e.error_len().unwrap_or(rest.len() - valid);
                for _ in 0..bad {
                    out.push('\u{FFFD}');
                }
                rest = &rest[valid + bad..];
            }
        }
    }
}

pub(crate) fn fs_read(args: &Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
    if super::fs_lines_range::has_range(args) {
        return super::fs_lines_range::range(args, cancel);
    }
    read_file("fs.read", args)
}

#[cfg(test)]
pub(crate) fn fs_download(args: &Args) -> CapResult {
    download(args, &tokio_util::sync::CancellationToken::new())
}

pub(crate) fn download(args: &Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
    if args.contains_key("offset") || args.contains_key("length") {
        return download_chunk(args, cancel);
    }
    read_file("fs.download", args)
}

fn chunk_arg(args: &Args, key: &str, min: i64, missing: u64) -> Result<u64, HandlerError> {
    if !args.contains_key(key) {
        return Ok(missing);
    }
    match int_arg(args, key) {
        Some(n) if n >= min => Ok(crate::util::positive_u64(n)),
        _ => Err(err(format!("fs.download: '{key}' must be {min} or more"))),
    }
}

fn download_chunk(args: &Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
    let kind = "fs.download";
    let path = local_path(kind, &str_arg(args, "path").unwrap_or_default())?;
    guard_read_path(kind, &path)?;
    let offset = chunk_arg(args, "offset", 0, 0)?;
    let length = chunk_arg(args, "length", 1, CHUNK_CAP)?.min(CHUNK_CAP);

    let info = stat(&path).map_err(|e| read_err(kind, "GetFileAttributesEx", &path, &e))?;
    if info.dir {
        return Err(err(format!("{kind}: {path} is a folder, not a file")));
    }
    let mut f = fs::File::open(&path).map_err(|e| read_err(kind, "open", &path, &e))?;
    let md = f
        .metadata()
        .map_err(|e| read_err(kind, "open", &path, &e))?;
    let size = md.len();
    let mtime = md.modified().unwrap_or(UNIX_EPOCH);
    if offset > size {
        return Err(err(format!(
            "{kind}: offset {offset} is past the end of {path}, which is {size} bytes"
        )));
    }
    let want = length.min(size - offset);
    let mut buf = vec![0u8; crate::util::size_usize(want)];
    f.seek(SeekFrom::Start(offset))
        .and_then(|_| f.read_exact(&mut buf))
        .map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                err(format!(
                    "{kind}: {path} got shorter while it was being read"
                ))
            } else {
                path_err("read", &path, &e)
            }
        })?;
    let eof = offset + want >= size;

    let mut out = json!({
        "path": path,
        "size": size,
        "mtime": fmt_time(mtime),
        "offset": offset,
        "length": want,
        "eof": eof,
        "base64": base64::engine::general_purpose::STANDARD.encode(&buf),
    });
    if eof {
        drop(buf);
        out["sha256"] = Value::String(hash_file(&mut f, &path, cancel)?);
    }
    Ok(out)
}

fn hash_file(
    f: &mut fs::File,
    path: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, HandlerError> {
    f.seek(SeekFrom::Start(0))
        .map_err(|e| path_err("read", path, &e))?;
    let mut hasher = Sha256::new();
    let mut block = vec![0u8; 1 << 20];
    loop {
        if cancel.is_cancelled() {
            return Err(err("command cancelled"));
        }
        let n = match f.read(&mut block) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(path_err("read", path, &e)),
        };
        hasher.update(&block[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn read_file(kind: &str, args: &Args) -> CapResult {
    let path = local_path(kind, &str_arg(args, "path").unwrap_or_default())?;
    guard_read_path(kind, &path)?;

    let info = stat(&path).map_err(|e| read_err(kind, "GetFileAttributesEx", &path, &e))?;
    if info.dir {
        return Err(err(format!("{kind}: {path} is a folder, not a file")));
    }
    if info.size > READ_CAP {
        let instead = if kind == "fs.read" {
            "Read part of it with start_line and end_line, fs.head, fs.tail or fs.grep, \
             or download it."
        } else {
            "Download it in parts with offset and length."
        };
        return Err(err(format!(
            "{kind}: {path} is {:.1} MiB, over the {} MiB limit. {instead}",
            info.size.to_f64().unwrap_or(f64::MAX) / (1024.0 * 1024.0),
            READ_CAP / (1024 * 1024)
        )));
    }

    let mut f = fs::File::open(&path).map_err(|e| read_err(kind, "open", &path, &e))?;
    let mut buf = vec![0u8; crate::util::size_usize(info.size)];
    f.read_exact(&mut buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            err("unexpected EOF")
        } else {
            err(e.to_string())
        }
    })?;

    Ok(json!({
        "path": path,
        "size": info.size,
        "sha256": hex(&Sha256::digest(&buf)),
        "base64": base64::engine::general_purpose::STANDARD.encode(&buf),
        "mtime": fmt_time(info.mtime),
    }))
}

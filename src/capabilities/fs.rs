use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::dispatch::{handler, Args, CapResult, HandlerError, Router};

const READ_CAP: u64 = 8 * 1024 * 1024;
const CHUNK_CAP: u64 = 4 * 1024 * 1024;
const SEARCH_VISIT_CAP: u64 = 200_000;
const DU_ENTRY_CAP: i64 = 1_000_000;

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/fs.json"),
        vec![
            ("fs.ls", blocking_handler(fs_ls)),
            (
                "fs.read",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || fs_read(args, &cancel)).await
                }),
            ),
            (
                "fs.search",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || fs_search(args, &cancel)).await
                }),
            ),
            (
                "fs.head",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || super::fs_lines::head(args, &cancel)).await
                }),
            ),
            (
                "fs.tail",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || super::fs_lines::tail(args, &cancel)).await
                }),
            ),
            (
                "fs.grep",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || super::fs_lines::grep(args, &cancel)).await
                }),
            ),
            ("fs.stat", blocking_handler(fs_stat)),
            ("fs.tree", blocking_handler(fs_tree)),
            (
                "fs.du",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || fs_du(args, &cancel)).await
                }),
            ),
        ],
    );
}

pub(crate) fn blocking_handler<F>(f: F) -> crate::dispatch::Handler
where
    F: Fn(Args) -> CapResult + Send + Sync + Clone + 'static,
{
    handler(move |_ctx, args| {
        let f = f.clone();
        async move { run_blocking(move || f(args)).await }
    })
}

pub(crate) async fn run_blocking<T, F>(f: F) -> Result<T, HandlerError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, HandlerError> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => v,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(_) => Err(HandlerError::plain("task cancelled")),
    }
}

pub(crate) fn str_arg(args: &Args, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

pub(crate) fn int_arg(args: &Args, key: &str) -> Option<i64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    }
}

pub(crate) fn bool_arg(args: &Args, key: &str) -> Option<bool> {
    match args.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

pub(crate) fn err(msg: impl Into<String>) -> HandlerError {
    HandlerError::plain(msg)
}

pub(crate) fn path_err(op: &str, path: &str, e: &io::Error) -> HandlerError {
    let text = e.to_string();
    let text = match text.rfind(" (os error ") {
        Some(i) if text.ends_with(')') => text[..i].to_string(),
        _ => text,
    };
    HandlerError::plain(format!("{op} {path}: {text}"))
}

pub(crate) fn read_err(kind: &str, op: &str, path: &str, e: &io::Error) -> HandlerError {
    match e.kind() {
        io::ErrorKind::NotFound => err(format!("{kind}: {path} doesn't exist")),
        io::ErrorKind::PermissionDenied => {
            err(format!("{kind}: Windows won't let Handoff open {path}"))
        }
        _ => path_err(op, path, e),
    }
}

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

pub(crate) use super::fs_guard::guard_read_path;

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

fn os_basename(name: &str) -> String {
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

pub(crate) fn fmt_time(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => {
            let d = e.duration();
            -(d.as_secs() as i64) - if d.subsec_nanos() > 0 { 1 } else { 0 }
        }
    };
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

const ATTR_READONLY: u32 = 0x1;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE: u32 = 0x400;
const TAG_SYMLINK: u32 = 0xA000000C;
const TAG_MOUNT_POINT: u32 = 0xA0000003;
const TAG_DEDUP: u32 = 0x80000013;

#[cfg(windows)]
fn attrs_of(md: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}

#[cfg(not(windows))]
fn attrs_of(md: &fs::Metadata) -> u32 {
    if md.is_dir() {
        ATTR_DIRECTORY
    } else {
        0
    }
}

#[cfg(windows)]
fn reparse_tag(path: &Path) -> u32 {
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct FindData {
        attrs: u32,
        times: [u32; 6],
        size_high: u32,
        size_low: u32,
        reserved0: u32,
        reserved1: u32,
        file_name: [u16; 260],
        alt_name: [u16; 14],
    }

    extern "system" {
        fn FindFirstFileW(name: *const u16, data: *mut FindData) -> isize;
        fn FindClose(handle: isize) -> i32;
    }

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut data = FindData {
        attrs: 0,
        times: [0; 6],
        size_high: 0,
        size_low: 0,
        reserved0: 0,
        reserved1: 0,
        file_name: [0; 260],
        alt_name: [0; 14],
    };
    let handle = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if handle == -1 {
        return 0;
    }
    unsafe { FindClose(handle) };
    data.reserved0
}

#[cfg(not(windows))]
fn reparse_tag(_path: &Path) -> u32 {
    0
}

pub(crate) struct Info {
    pub size: u64,
    pub mtime: SystemTime,
    pub dir: bool,
    pub symlink: bool,
    pub mode: String,
}

pub(crate) fn info_of(md: &fs::Metadata, path: &Path) -> Info {
    let attrs = attrs_of(md);
    let mut perm: u32 = if attrs & ATTR_READONLY != 0 {
        0o444
    } else {
        0o666
    };
    let tag = if attrs & ATTR_REPARSE != 0 {
        reparse_tag(path)
    } else {
        0
    };
    let mut dir = false;
    let mut irregular = false;
    let symlink = tag == TAG_SYMLINK || tag == TAG_MOUNT_POINT;
    if !symlink {
        if attrs & ATTR_DIRECTORY != 0 {
            dir = true;
            perm |= 0o111;
        }
        if tag != 0 && tag != TAG_DEDUP {
            irregular = true;
        }
    }
    let mut mode = String::new();
    if dir {
        mode.push('d');
    }
    if symlink {
        mode.push('L');
    }
    if irregular {
        mode.push('?');
    }
    if mode.is_empty() {
        mode.push('-');
    }
    for (i, c) in "rwxrwxrwx".chars().enumerate() {
        mode.push(if perm & (1 << (8 - i)) != 0 { c } else { '-' });
    }
    Info {
        size: md.len(),
        mtime: md.modified().unwrap_or(UNIX_EPOCH),
        dir,
        symlink,
        mode,
    }
}

pub(crate) fn stat(path: &str) -> io::Result<Info> {
    let md = fs::metadata(path)?;
    Ok(info_of(&md, Path::new(path)))
}

pub(crate) fn lstat(path: &str) -> io::Result<Info> {
    let md = fs::symlink_metadata(path)?;
    Ok(info_of(&md, Path::new(path)))
}

fn fs_ls(args: Args) -> CapResult {
    let path = local_path("fs.ls", &str_arg(&args, "path").unwrap_or_default())?;

    let rd = fs::read_dir(&path).map_err(|e| {
        if Path::new(&path).is_file() {
            err(format!("fs.ls: {path} is a file, not a folder"))
        } else {
            let cause = fs::metadata(&path).err().unwrap_or(e);
            read_err("fs.ls", "open", &path, &cause)
        }
    })?;
    let mut items: Vec<(String, fs::DirEntry)> = Vec::new();
    for ent in rd {
        let ent = match ent {
            Ok(e) => e,
            Err(e) => return Err(path_err("Readdir", &path, &e)),
        };
        items.push((ent.file_name().to_string_lossy().into_owned(), ent));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::with_capacity(items.len());
    for (name, ent) in &items {
        let Ok(md) = ent.metadata() else { continue };
        let info = info_of(&md, &ent.path());
        out.push(json!({
            "name": name,
            "dir": info.dir,
            "size": info.size,
            "mode": info.mode,
            "mtime": fmt_time(info.mtime),
        }));
    }
    Ok(json!({"path": path, "entries": out}))
}

pub(crate) fn fs_read(args: Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
    if super::fs_lines::has_range(&args) {
        return super::fs_lines::range(args, cancel);
    }
    read_file("fs.read", args)
}

#[cfg(test)]
pub(crate) fn fs_download(args: Args) -> CapResult {
    download(args, &tokio_util::sync::CancellationToken::new())
}

pub(crate) fn download(args: Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
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
        Some(n) if n >= min => Ok(n as u64),
        _ => Err(err(format!("fs.download: '{key}' must be {min} or more"))),
    }
}

fn download_chunk(args: Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
    let kind = "fs.download";
    let path = local_path(kind, &str_arg(&args, "path").unwrap_or_default())?;
    guard_read_path(kind, &path)?;
    let offset = chunk_arg(&args, "offset", 0, 0)?;
    let length = chunk_arg(&args, "length", 1, CHUNK_CAP)?.min(CHUNK_CAP);

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
    let mut buf = vec![0u8; want as usize];
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

fn read_file(kind: &str, args: Args) -> CapResult {
    let path = local_path(kind, &str_arg(&args, "path").unwrap_or_default())?;
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
            info.size as f64 / (1024.0 * 1024.0),
            READ_CAP / (1024 * 1024)
        )));
    }

    let mut f = fs::File::open(&path).map_err(|e| read_err(kind, "open", &path, &e))?;
    let mut buf = vec![0u8; info.size as usize];
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

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.') && name.len() > 1
}

pub(crate) enum Flow {
    Continue,
    SkipDir,
    Stop,
}

pub(crate) struct Node {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub info: Option<Info>,
}

pub(crate) fn walk_dir(root: &str, f: &mut dyn FnMut(&Node) -> Flow) {
    let Ok(md) = fs::symlink_metadata(root) else {
        return;
    };
    let info = info_of(&md, Path::new(root));
    let node = Node {
        path: root.to_string(),
        name: os_basename(root),
        is_dir: info.dir,
        info: Some(info),
    };
    walk_node(&node, f);
}

fn walk_node(node: &Node, f: &mut dyn FnMut(&Node) -> Flow) -> bool {
    match f(node) {
        Flow::Stop => return false,
        Flow::SkipDir => return true,
        Flow::Continue => {}
    }
    if !node.is_dir {
        return true;
    }
    let Ok(rd) = fs::read_dir(&node.path) else {
        return true;
    };
    let mut items: Vec<fs::DirEntry> = rd.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name().to_string_lossy().into_owned());
    for ent in items {
        let name = ent.file_name().to_string_lossy().into_owned();
        let path = go_join(&node.path, &name);
        let info = ent.metadata().ok().map(|md| info_of(&md, &ent.path()));
        let is_dir = match &info {
            Some(i) => i.dir,
            None => ent.file_type().map(|t| t.is_dir()).unwrap_or(false),
        };
        let child = Node {
            path,
            name,
            is_dir,
            info,
        };
        if !walk_node(&child, f) {
            return false;
        }
    }
    true
}

fn slash_depth(path: &str) -> i64 {
    path.replace('\\', "/").split('/').count() as i64
}

fn fs_search(args: Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
    search(args, cancel, SEARCH_VISIT_CAP)
}

fn search(args: Args, cancel: &tokio_util::sync::CancellationToken, visit_cap: u64) -> CapResult {
    let search_path = local_path("fs.search", &str_arg(&args, "path").unwrap_or_default())?;

    let mut pattern = str_arg(&args, "pattern").unwrap_or_else(|| "*".to_string());
    if pattern.is_empty() {
        pattern = "*".to_string();
    }
    let pattern = pattern.trim().to_string();
    if let Err(e) = glob_match(&pattern, "probe.txt") {
        return Err(err(format!("fs.search: bad pattern: {e}")));
    }

    let mut max_results = int_arg(&args, "max_results").unwrap_or(200);
    if max_results <= 0 {
        max_results = 200;
    }
    if max_results > 2000 {
        max_results = 2000;
    }
    let mut max_depth = int_arg(&args, "max_depth").unwrap_or(4);
    max_depth = max_depth.clamp(0, 20);
    let include_dirs = bool_arg(&args, "include_dirs").unwrap_or(false);
    let include_hidden = bool_arg(&args, "include_hidden").unwrap_or(false);

    let root = stat(&search_path)
        .map_err(|e| read_err("fs.search", "GetFileAttributesEx", &search_path, &e))?;
    if !root.dir {
        return Err(err(format!(
            "fs.search: {search_path:?} is not a directory"
        )));
    }

    let lower_pattern = pattern.to_lowercase();
    let root_depth = slash_depth(&go_clean(&search_path));
    let mut entries: Vec<Value> = Vec::new();
    let mut visited: u64 = 0;
    let mut visit_cap_hit = false;

    walk_dir(&search_path, &mut |node| {
        visited += 1;
        if visited > visit_cap || cancel.is_cancelled() {
            visit_cap_hit = visited > visit_cap;
            visited -= 1;
            return Flow::Stop;
        }
        let name = node.name.as_str();
        if name == "." || name.is_empty() {
            return Flow::Continue;
        }
        let is_dir = node.is_dir;
        if is_hidden_name(name) && !include_hidden {
            return if is_dir {
                Flow::SkipDir
            } else {
                Flow::Continue
            };
        }
        let depth = slash_depth(&node.path) - root_depth;
        if depth > max_depth {
            return if is_dir {
                Flow::SkipDir
            } else {
                Flow::Continue
            };
        }
        if depth == 0 && !include_dirs {
            return Flow::Continue;
        }
        match glob_match(&lower_pattern, &name.to_lowercase()) {
            Ok(true) => {}
            _ => return Flow::Continue,
        }
        let Some(info) = &node.info else {
            return Flow::Continue;
        };
        let size = if is_dir { 0 } else { info.size };
        entries.push(json!({
            "path": node.path,
            "name": name,
            "dir": is_dir,
            "size": size,
            "mtime": fmt_time(info.mtime),
        }));
        if entries.len() as i64 >= max_results {
            return Flow::Stop;
        }
        Flow::Continue
    });

    let count = entries.len();
    let mut out = json!({
        "path": search_path,
        "pattern": pattern,
        "max_depth": max_depth,
        "max_results": max_results,
        "include_dirs": include_dirs,
        "include_hidden": include_hidden,
        "count": count,
        "visited": visited,
        "truncated": visit_cap_hit || count as i64 >= max_results,
        "entries": entries,
    });
    if visit_cap_hit {
        out["note"] = json!(format!(
            "stopped after looking at {visit_cap} files and folders; search a smaller folder or lower max_depth"
        ));
    }
    Ok(out)
}

fn fs_stat(args: Args) -> CapResult {
    let path = local_path("fs.stat", &str_arg(&args, "path").unwrap_or_default())?;

    let info =
        lstat(&path).map_err(|e| read_err("fs.stat", "GetFileAttributesEx", &path, &e))?;
    let mut target = String::new();
    if info.symlink {
        if let Ok(dest) = fs::read_link(&path) {
            target = dest.to_string_lossy().into_owned();
        }
    }
    Ok(json!({
        "path": go_clean(&path),
        "name": go_base(&path),
        "size": info.size,
        "mtime": fmt_time(info.mtime),
        "mode": info.mode,
        "is_dir": info.dir,
        "is_symlink": info.symlink,
        "symlink_target": target,
    }))
}

fn fs_tree(args: Args) -> CapResult {
    let path = local_path("fs.tree", &str_arg(&args, "path").unwrap_or_default())?;

    let max_depth = int_arg(&args, "max_depth").unwrap_or(3).clamp(1, 8);
    let max_entries = int_arg(&args, "max_entries").unwrap_or(500).clamp(50, 5000);

    let root = stat(&path).map_err(|e| read_err("fs.tree", "GetFileAttributesEx", &path, &e))?;
    if !root.dir {
        return Err(err(format!("fs.tree: {path:?} is not a directory")));
    }

    let root_depth = slash_depth(&go_clean(&path));
    let mut entries: Vec<Value> = Vec::new();

    walk_dir(&path, &mut |node| {
        if node.path == path {
            return Flow::Continue;
        }
        let depth = slash_depth(&node.path) - root_depth;
        if depth > max_depth {
            return if node.is_dir {
                Flow::SkipDir
            } else {
                Flow::Continue
            };
        }
        let Some(info) = &node.info else {
            return Flow::Continue;
        };
        let mut entry = Map::new();
        entry.insert("path".into(), json!(node.path));
        entry.insert("name".into(), json!(node.name));
        entry.insert("depth".into(), json!(depth));
        entry.insert("dir".into(), json!(node.is_dir));
        entry.insert("size".into(), json!(info.size));
        entries.push(Value::Object(entry));
        if entries.len() as i64 >= max_entries {
            return Flow::Stop;
        }
        Flow::Continue
    });

    let count = entries.len();
    Ok(json!({
        "path": path,
        "max_depth": max_depth,
        "max_entries": max_entries,
        "count": count,
        "truncated": count as i64 >= max_entries,
        "entries": entries,
    }))
}

#[derive(Default)]
struct DuChild {
    dir: bool,
    bytes: u64,
    files: u64,
}

fn fs_du(args: Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
    let path = local_path("fs.du", &str_arg(&args, "path").unwrap_or_default())?;
    let top = int_arg(&args, "top").unwrap_or(20).clamp(1, 200) as usize;
    let max_entries = int_arg(&args, "max_entries")
        .unwrap_or(500_000)
        .clamp(1000, DU_ENTRY_CAP);

    let root = stat(&path).map_err(|e| read_err("fs.du", "GetFileAttributesEx", &path, &e))?;
    if !root.dir {
        return Err(err(format!("fs.du: {path:?} is not a directory")));
    }

    let prefix_len = path.trim_end_matches(['/', '\\']).len() + 1;
    let mut children: std::collections::HashMap<String, DuChild> = Default::default();
    let (mut bytes, mut files, mut dirs, mut seen) = (0u64, 0u64, 0u64, 0i64);
    let mut stopped = false;
    walk_dir(&path, &mut |node| {
        if node.path == path {
            return Flow::Continue;
        }
        seen += 1;
        if seen > max_entries || cancel.is_cancelled() {
            stopped = true;
            return Flow::Stop;
        }
        let rel = node.path.get(prefix_len..).unwrap_or_default();
        let first = rel.split(['/', '\\']).next().unwrap_or(rel);
        let child = children.entry(first.to_string()).or_default();
        if first.len() == rel.len() {
            child.dir = node.is_dir;
        }
        if node.is_dir {
            dirs += 1;
        } else {
            let size = node.info.as_ref().map_or(0, |i| i.size);
            bytes += size;
            files += 1;
            child.bytes += size;
            child.files += 1;
        }
        Flow::Continue
    });

    let mut rows: Vec<(String, DuChild)> = children.into_iter().collect();
    rows.sort_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then_with(|| a.0.cmp(&b.0)));
    let rest: u64 = rows.iter().skip(top).map(|(_, c)| c.bytes).sum();
    let entries: Vec<Value> = rows
        .iter()
        .take(top)
        .map(|(name, c)| {
            json!({
                "name": name,
                "path": go_join(&path, name),
                "dir": c.dir,
                "bytes": c.bytes,
                "files": c.files,
            })
        })
        .collect();
    Ok(json!({
        "path": path,
        "bytes": bytes,
        "files": files,
        "dirs": dirs,
        "truncated": stopped,
        "max_entries": max_entries,
        "other_bytes": rest,
        "entries": entries,
    }))
}

fn decode_rune(b: &[u8]) -> (char, usize) {
    if b.is_empty() {
        return ('\u{FFFD}', 0);
    }
    let want = match b[0] {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return ('\u{FFFD}', 1),
    };
    if b.len() >= want {
        if let Ok(s) = std::str::from_utf8(&b[..want]) {
            if let Some(c) = s.chars().next() {
                return (c, want);
            }
        }
    }
    ('\u{FFFD}', 1)
}

const BAD_PATTERN: &str = "syntax error in pattern";

pub(crate) fn glob_match(pattern: &str, name: &str) -> Result<bool, &'static str> {
    let mut pattern = pattern.as_bytes();
    let mut name = name.as_bytes();
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            return Ok(!name.contains(&b'\\'));
        }
        let first = match_chunk(chunk, name);
        if let Ok(Some(t)) = first {
            if t.is_empty() || !pattern.is_empty() {
                name = t;
                continue;
            }
        }
        first?;
        if star {
            let mut i = 0;
            while i < name.len() && name[i] != b'\\' {
                match match_chunk(chunk, &name[i + 1..]) {
                    Ok(Some(t)) => {
                        if pattern.is_empty() && !t.is_empty() {
                            i += 1;
                            continue;
                        }
                        name = t;
                        continue 'pattern;
                    }
                    Ok(None) => {}
                    Err(e) => return Err(e),
                }
                i += 1;
            }
        }
        while !pattern.is_empty() {
            let (_, chunk, rest) = scan_chunk(pattern);
            pattern = rest;
            match_chunk(chunk, b"")?;
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

fn scan_chunk(mut pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut star = false;
    while !pattern.is_empty() && pattern[0] == b'*' {
        pattern = &pattern[1..];
        star = true;
    }
    let mut inrange = false;
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'[' => inrange = true,
            b']' => inrange = false,
            b'*' if !inrange => break,
            _ => {}
        }
        i += 1;
    }
    (star, &pattern[..i], &pattern[i..])
}

fn get_esc(chunk: &[u8]) -> Result<(char, &[u8]), &'static str> {
    if chunk.is_empty() || chunk[0] == b'-' || chunk[0] == b']' {
        return Err(BAD_PATTERN);
    }
    let (r, n) = decode_rune(chunk);
    if r == '\u{FFFD}' && n == 1 {
        return Err(BAD_PATTERN);
    }
    let rest = &chunk[n..];
    if rest.is_empty() {
        return Err(BAD_PATTERN);
    }
    Ok((r, rest))
}

fn match_chunk<'a>(mut chunk: &[u8], mut s: &'a [u8]) -> Result<Option<&'a [u8]>, &'static str> {
    let mut failed = false;
    while !chunk.is_empty() {
        if !failed && s.is_empty() {
            failed = true;
        }
        match chunk[0] {
            b'[' => {
                let mut r = '\u{FFFD}';
                if !failed {
                    let (c, n) = decode_rune(s);
                    r = c;
                    s = &s[n..];
                }
                chunk = &chunk[1..];
                let mut negated = false;
                if !chunk.is_empty() && chunk[0] == b'^' {
                    negated = true;
                    chunk = &chunk[1..];
                }
                let mut matched = false;
                let mut nrange = 0;
                loop {
                    if !chunk.is_empty() && chunk[0] == b']' && nrange > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let (lo, rest) = get_esc(chunk)?;
                    chunk = rest;
                    let mut hi = lo;
                    if chunk[0] == b'-' {
                        let (h, rest) = get_esc(&chunk[1..])?;
                        hi = h;
                        chunk = rest;
                    }
                    if lo <= r && r <= hi {
                        matched = true;
                    }
                    nrange += 1;
                }
                if matched == negated {
                    failed = true;
                }
            }
            b'?' => {
                if !failed {
                    if s[0] == b'\\' {
                        failed = true;
                    }
                    let (_, n) = decode_rune(s);
                    s = &s[n..];
                }
                chunk = &chunk[1..];
            }
            _ => {
                if !failed {
                    if chunk[0] != s[0] {
                        failed = true;
                    }
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    if failed {
        return Ok(None);
    }
    Ok(Some(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::fs_guard::is_secret_path;
    use crate::dispatch::Ctx;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn router() -> Router {
        let mut r = Router::new();
        register(&mut r);
        r
    }

    async fn run(kind: &str, v: Value) -> crate::dispatch::Outcome {
        router().dispatch(Ctx::default(), kind, args(v)).await
    }

    fn lines(out: &crate::dispatch::Outcome) -> Vec<String> {
        out.result["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    fn numbered(dir: &Path) -> String {
        let target = dir.join("log.txt");
        let mut s = String::new();
        for i in 0..200 {
            s.push_str(&format!("line-{i}\n"));
        }
        fs::write(&target, s).unwrap();
        target.to_string_lossy().into_owned()
    }

    #[test]
    fn registers_the_read_only_fs_kinds() {
        assert_eq!(
            router().kinds(),
            vec![
                "fs.du",
                "fs.grep",
                "fs.head",
                "fs.ls",
                "fs.read",
                "fs.search",
                "fs.stat",
                "fs.tail",
                "fs.tree"
            ]
        );
    }

    #[test]
    fn clean_matches_go_on_windows_paths() {
        assert_eq!(go_clean("C:/a/./b/../c//"), "C:\\a\\c");
        assert_eq!(go_clean("C:\\"), "C:\\");
        assert_eq!(go_clean("C:"), "C:.");
        assert_eq!(go_clean("a/../.."), "..");
        assert_eq!(go_clean("\\\\host\\share\\x\\.."), "\\\\host\\share\\");
        assert_eq!(go_clean("C:\\..\\x"), "C:\\x");
        assert!(local_path("fs.ls", "C:\\x").is_ok());
        assert!(local_path("fs.ls", "\\x").is_err());
        assert!(local_path("fs.ls", "C:x").is_err());
        assert!(local_path("fs.ls", "relative").is_err());
        assert_eq!(go_base("C:\\"), "\\");
        assert_eq!(go_base("C:\\a\\b.txt"), "b.txt");
    }

    #[test]
    fn glob_matches_like_go() {
        assert_eq!(glob_match("*.txt", "a.txt"), Ok(true));
        assert_eq!(glob_match("*.txt", "a.log"), Ok(false));
        assert_eq!(glob_match("l?g-[0-9]*", "log-5.x"), Ok(true));
        assert_eq!(glob_match("[^a]b", "ab"), Ok(false));
        assert_eq!(glob_match("*", "anything"), Ok(true));
        assert!(glob_match("[", "x").is_err());
        assert!(glob_match("a[", "b").is_err());
    }

    #[test]
    fn time_format_is_utc_seconds() {
        assert_eq!(fmt_time(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            fmt_time(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z"
        );
    }

    #[tokio::test]
    async fn search_rejects_relative_path() {
        let out = run(
            "fs.search",
            json!({"path": "relative/path", "pattern": "*.txt"}),
        )
        .await;
        assert!(!out.ok);
        assert!(out.error.contains("absolute"), "{}", out.error);
    }

    #[tokio::test]
    async fn search_matches_pattern_and_respects_limits() {
        let dir = tempfile::Builder::new()
            .prefix("fs-search")
            .tempdir()
            .unwrap();
        let root = dir.path();
        for i in 0..120 {
            fs::write(root.join(format!("log-{i}.txt")), "x").unwrap();
            if i == 0 {
                fs::create_dir_all(root.join(".hidden")).unwrap();
                fs::write(root.join(".hidden").join("secret.log"), "x").unwrap();
            }
        }
        let out = run(
            "fs.search",
            json!({"path": root, "pattern": "*.txt", "max_results": 20}),
        )
        .await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["count"], json!(20));
        let entries = out.result["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 20);
        for row in entries {
            assert!(!row["name"].as_str().unwrap().starts_with('.'));
        }

        let hidden = run(
            "fs.search",
            json!({"path": root, "pattern": "*.log", "include_hidden": true}),
        )
        .await;
        assert!(hidden.ok, "{}", hidden.error);
        assert_eq!(hidden.result["count"], json!(1));

        let none = run("fs.search", json!({"path": root, "pattern": "*.log"})).await;
        assert_eq!(none.result["count"], json!(0));

        let bad = run("fs.search", json!({"path": root, "pattern": "["})).await;
        assert!(!bad.ok);
        assert!(
            bad.error.starts_with("fs.search: bad pattern"),
            "{}",
            bad.error
        );
    }

    #[tokio::test]
    async fn head_and_tail_reject_relative_path() {
        for kind in ["fs.head", "fs.tail"] {
            let out = run(kind, json!({"path": "relative"})).await;
            assert!(!out.ok);
            assert!(out.error.contains("absolute"), "{}", out.error);
        }
    }

    #[tokio::test]
    async fn head_returns_line_window() {
        let dir = tempfile::tempdir().unwrap();
        let target = numbered(dir.path());
        let out = run("fs.head", json!({"path": target, "lines": 10})).await;
        assert!(out.ok, "{}", out.error);
        let got = lines(&out);
        assert_eq!(got.len(), 10);
        assert_eq!(got[0], "line-0");
        assert_eq!(got[9], "line-9");
        assert_eq!(out.result["mode"], json!("head"));
    }

    #[tokio::test]
    async fn tail_returns_trailing_lines() {
        let dir = tempfile::tempdir().unwrap();
        let target = numbered(dir.path());
        let out = run("fs.tail", json!({"path": target, "lines": 5})).await;
        assert!(out.ok, "{}", out.error);
        let got = lines(&out);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0], "line-195");
        assert_eq!(got[4], "line-199");
        assert_eq!(out.result["line_count"], json!(5));
    }

    #[tokio::test]
    async fn tail_of_a_large_file_starts_on_a_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("big.log");
        let mut s = String::new();
        for i in 0..40000 {
            s.push_str(&format!("entry-{i:06}\n"));
        }
        fs::write(&target, &s).unwrap();
        let out = run("fs.tail", json!({"path": target, "lines": 3})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(
            lines(&out),
            vec!["entry-039997", "entry-039998", "entry-039999"]
        );
        assert_eq!(out.result["truncated"], json!(false));
    }

    #[tokio::test]
    async fn stat_returns_file_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("thing.txt");
        fs::write(&target, "hello").unwrap();
        let out = run("fs.stat", json!({"path": target})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["name"], json!("thing.txt"));
        assert_eq!(out.result["is_dir"], json!(false));
        assert_eq!(out.result["size"], json!(5));
        assert_eq!(out.result["mode"], json!("-rw-rw-rw-"));
    }

    #[tokio::test]
    async fn tree_returns_entries_with_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            let sub = dir.path().join(format!("sub-{i}"));
            fs::create_dir_all(&sub).unwrap();
            for j in 0..5 {
                fs::write(sub.join(format!("f-{j}.txt")), "x").unwrap();
            }
        }
        let out = run(
            "fs.tree",
            json!({"path": dir.path(), "max_depth": 2, "max_entries": 100}),
        )
        .await;
        assert!(out.ok, "{}", out.error);
        let count = out.result["count"].as_i64().unwrap();
        assert!(count > 0 && count <= 100, "count = {count}");
        assert_eq!(count, 60);
        assert_eq!(out.result["truncated"], json!(false));

        let capped = run(
            "fs.tree",
            json!({"path": dir.path(), "max_depth": 2, "max_entries": 50}),
        )
        .await;
        assert_eq!(capped.result["count"], json!(50));
        assert_eq!(capped.result["truncated"], json!(true));
    }

    #[tokio::test]
    async fn ls_and_read_report_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.txt"), "hi").unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        let out = run("fs.ls", json!({"path": dir.path()})).await;
        assert!(out.ok, "{}", out.error);
        let entries = out.result["entries"].as_array().unwrap();
        assert_eq!(entries[0]["name"], json!("a"));
        assert_eq!(entries[0]["dir"], json!(true));
        assert_eq!(entries[1]["name"], json!("b.txt"));
        assert_eq!(entries[1]["size"], json!(2));

        let read = run("fs.read", json!({"path": dir.path().join("b.txt")})).await;
        assert!(read.ok, "{}", read.error);
        assert_eq!(read.result["base64"], json!("aGk="));
        assert_eq!(
            read.result["sha256"],
            json!("8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4")
        );

        let dir_read = run("fs.read", json!({"path": dir.path()})).await;
        assert!(!dir_read.ok);
        assert!(dir_read.error.ends_with("is a folder, not a file"));

        let sensitive = run(
            "fs.read",
            json!({"path": "C:\\Windows\\System32\\config\\SAM"}),
        )
        .await;
        assert!(
            sensitive.error.contains("can hold passwords or keys"),
            "{}",
            sensitive.error
        );
    }

    #[tokio::test]
    async fn du_totals_a_folder_and_ranks_its_children() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("big").join("inner")).unwrap();
        fs::write(
            dir.path().join("big").join("inner").join("a.bin"),
            vec![0u8; 3000],
        )
        .unwrap();
        fs::write(dir.path().join("small.txt"), vec![0u8; 10]).unwrap();
        let out = run("fs.du", json!({"path": dir.path()})).await;
        assert!(out.ok, "{}", out.error);
        let r = &out.result;
        assert_eq!(r["bytes"], json!(3010));
        assert_eq!(r["files"], json!(2));
        assert_eq!(r["dirs"], json!(2));
        assert_eq!(r["truncated"], json!(false));
        assert_eq!(r["entries"][0]["name"], json!("big"));
        assert_eq!(r["entries"][0]["dir"], json!(true));
        assert_eq!(r["entries"][0]["bytes"], json!(3000));
        assert_eq!(r["entries"][1]["name"], json!("small.txt"));
        assert_eq!(r["entries"][1]["dir"], json!(false));

        let top = run("fs.du", json!({"path": dir.path(), "top": 1})).await;
        assert_eq!(top.result["entries"].as_array().unwrap().len(), 1);
        assert_eq!(top.result["other_bytes"], json!(10));
    }

    #[tokio::test]
    async fn du_rejects_relative_paths() {
        let out = run("fs.du", json!({"path": "relative/dir"})).await;
        assert!(!out.ok);
        assert!(out.error.contains("absolute"), "{}", out.error);
    }

    const ALL_KINDS: [&str; 9] = [
        "fs.du",
        "fs.grep",
        "fs.head",
        "fs.ls",
        "fs.read",
        "fs.search",
        "fs.stat",
        "fs.tail",
        "fs.tree",
    ];

    #[tokio::test]
    async fn every_fs_kind_refuses_network_and_device_paths() {
        for path in [
            "\\\\attacker\\share\\x.txt",
            "//attacker/share/x.txt",
            "\\\\?\\C:\\Windows\\win.ini",
            "\\\\?\\UNC\\attacker\\share\\x",
            "\\\\.\\PhysicalDrive0",
            "\\\\.\\pipe\\x",
            "\\/attacker\\share",
        ] {
            for kind in ALL_KINDS {
                let out = run(kind, json!({ "path": path, "text": "x" })).await;
                assert!(!out.ok, "{kind} {path}");
                assert_eq!(
                    out.error,
                    format!("{kind}: network and device paths are not allowed"),
                    "{path}"
                );
            }
        }
        for path in [
            "\\??\\UNC\\attacker\\share\\x",
            "\\Device\\Mup\\attacker",
            "C:relative",
        ] {
            for kind in ALL_KINDS {
                let out = run(kind, json!({ "path": path, "text": "x" })).await;
                assert!(
                    out.error.contains("must be absolute"),
                    "{kind} {path}: {}",
                    out.error
                );
            }
        }
    }

    #[test]
    fn local_path_rejects_streams_and_device_names() {
        assert_eq!(local_path("fs.read", " C:/a/./b ").unwrap(), "C:\\a\\b");
        assert!(local_path("fs.read", "C:\\a\\..\\..\\b").is_ok());
        let e = local_path("fs.read", "C:\\Users\\x\\.ssh\\id_rsa::$DATA").unwrap_err();
        assert_eq!(
            e.message,
            "fs.read: ':' is only allowed after the drive letter"
        );
        for dev in [
            "CON",
            "nul.txt",
            "Aux",
            "COM1",
            "lpt9.log",
            "CONIN$",
            "com\u{b9}",
        ] {
            let e = local_path("fs.read", &format!("C:\\temp\\{dev}")).unwrap_err();
            assert_eq!(
                e.message,
                format!("fs.read: {dev} is a reserved device name")
            );
        }
        assert!(local_path("fs.read", "C:\\temp\\console.txt").is_ok());
        assert!(local_path("fs.read", "C:\\temp\\com10").is_ok());
        assert_eq!(
            local_path("fs.read", "  ").unwrap_err().message,
            "fs.read: 'path' is required"
        );
    }

    #[test]
    fn the_secret_list_matches_after_normalising() {
        for p in [
            "C:\\Windows\\System32\\config\\SAM",
            "c:/windows/system32/CONFIG/system",
            "D:\\WINDOWS\\System32\\Config\\SECURITY",
            "C:\\Windows\\System32\\config\\RegBack\\SAM",
            "C:\\Users\\bob\\.ssh\\id_rsa",
            "C:\\Users\\bob\\.SSH\\config",
            "C:\\Users\\bob\\.ssh.\\known_hosts",
            "C:\\Users\\bob\\.aws\\credentials",
            "C:\\Users\\bob\\.azure\\msal_token_cache.bin",
            "C:\\Users\\bob\\.git-credentials",
            "C:\\Users\\bob\\.git-credentials. ",
            "C:\\Users\\bob\\NTUSER.DAT",
            "C:\\Users\\bob\\ntuser.dat.LOG1",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\Windows\\UsrClass.dat",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Login Data",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Default\\Network\\Cookies",
            "C:\\Users\\bob\\AppData\\Local\\Google\\Chrome\\User Data\\Local State",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\Edge\\User Data\\Default\\Web Data",
            "C:\\Users\\bob\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\x.default\\logins.json",
            "C:\\Users\\bob\\AppData\\Roaming\\Mozilla\\Firefox\\Profiles\\x.default\\key4.db",
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Credentials\\ABCDEF",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\Credentials\\ABCDEF",
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Protect\\S-1-5-21\\key",
            "C:\\Users\\bob\\AppData\\Local\\Microsoft\\Vault\\x\\y.vcrd",
            "C:\\ProgramData\\Microsoft\\Crypto\\RSA\\MachineKeys\\k",
            "C:\\Users\\bob\\Documents\\passwords.kdbx",
            "C:\\Users\\bob\\Desktop\\cert.PFX",
            "C:\\Windows\\Panther\\unattend.xml",
            "C:\\pagefile.sys",
            "\\\\?\\C:\\Users\\bob\\.ssh\\id_ed25519",
            "C:\\Users\\bob\\keys\\id_ed25519_sk",
            "C:\\ProgramData\\ssh\\ssh_host_ed25519_key",
            "C:\\ProgramData\\ssh\\ssh_host_rsa_key.pub",
        ] {
            assert!(is_secret_path(p), "{p} was allowed");
        }
        for p in [
            "C:\\Users\\bob\\Documents\\notes.txt",
            "C:\\Windows\\System32\\drivers\\etc\\hosts",
            "C:\\Windows\\Logs\\CBS\\CBS.log",
            "C:\\Users\\bob\\sshfiles\\readme.txt",
            "C:\\Users\\bob\\Documents\\cookies recipe.txt",
            "C:\\Windows\\System32\\configuration.txt",
            "C:\\ProgramData\\ssh\\logs\\sshd.log",
        ] {
            assert!(!is_secret_path(p), "{p} was blocked");
        }
    }

    #[tokio::test]
    async fn every_read_path_refuses_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        let key = ssh.join("id_rsa");
        fs::write(&key, "PRIVATE").unwrap();
        let mut r = router();
        super::super::fs_write::register(&mut r);
        for kind in ["fs.read", "fs.head", "fs.tail", "fs.grep", "fs.download"] {
            let out = r
                .dispatch(
                    Ctx::default(),
                    kind,
                    args(json!({ "path": key, "text": "P" })),
                )
                .await;
            assert!(!out.ok, "{kind} read the key");
            assert!(
                out.error.ends_with("because it can hold passwords or keys"),
                "{kind}: {}",
                out.error
            );
            let upper = key.to_string_lossy().to_uppercase();
            let out = r
                .dispatch(
                    Ctx::default(),
                    kind,
                    args(json!({ "path": upper, "text": "P" })),
                )
                .await;
            assert!(!out.ok, "{kind} read the key by its upper-case name");
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_junction_does_not_get_around_the_secret_list() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        fs::write(ssh.join("id_rsa"), "PRIVATE").unwrap();
        let link = dir.path().join("innocent");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&ssh)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let sneaky = link.join("id_rsa");
        assert_eq!(fs::read_to_string(&sneaky).unwrap(), "PRIVATE");
        for kind in ["fs.read", "fs.head", "fs.tail"] {
            let out = run(kind, json!({ "path": sneaky })).await;
            assert!(!out.ok, "{kind} followed the junction");
            assert!(out.error.contains("passwords or keys"), "{}", out.error);
        }
    }

    #[test]
    fn search_stops_at_the_visit_cap_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plain");
        fs::create_dir(&dir).unwrap();
        for i in 0..30 {
            fs::write(dir.join(format!("f-{i:02}.txt")), "x").unwrap();
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let a = args(json!({"path": dir, "pattern": "*.txt", "max_results": 2000}));
        let out = search(a.clone(), &cancel, 10).unwrap();
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(out["visited"], json!(10));
        assert_eq!(out["count"], json!(9));
        assert_eq!(
            out["note"],
            json!("stopped after looking at 10 files and folders; search a smaller folder or lower max_depth")
        );
        let full = search(a, &cancel, SEARCH_VISIT_CAP).unwrap();
        assert_eq!(full["truncated"], json!(false));
        assert_eq!(full["count"], json!(30));
        assert!(full.get("note").is_none());
    }

    #[tokio::test]
    async fn du_entry_cap_is_one_million() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            "fs.du",
            json!({"path": dir.path(), "max_entries": 5_000_000}),
        )
        .await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["max_entries"], json!(DU_ENTRY_CAP));
        assert_eq!(out.clamped.len(), 1);
        assert_eq!(out.clamped[0].to, DU_ENTRY_CAP);
    }

    fn download_router() -> Router {
        let mut r = Router::new();
        super::super::fs_write::register(&mut r);
        r
    }

    async fn download_part(r: &Router, v: Value) -> crate::dispatch::Outcome {
        r.dispatch(Ctx::default(), "fs.download", args(v)).await
    }

    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn decode(out: &crate::dispatch::Outcome) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(out.result["base64"].as_str().unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn download_parts_cover_start_middle_and_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let data = patterned(10 * 1024 * 1024 + 123);
        fs::write(&path, &data).unwrap();
        let size = data.len() as u64;
        let mtime = fmt_time(fs::metadata(&path).unwrap().modified().unwrap());
        let r = download_router();

        let first = download_part(&r, json!({"path": path, "offset": 0})).await;
        assert!(first.ok, "{}", first.error);
        assert_eq!(decode(&first), &data[..CHUNK_CAP as usize]);
        assert_eq!(first.result["offset"], json!(0));
        assert_eq!(first.result["length"], json!(CHUNK_CAP));
        assert_eq!(first.result["eof"], json!(false));
        assert!(first.result.get("sha256").is_none());

        let mid_at = 4 * 1024 * 1024 + 5;
        let mid = download_part(&r, json!({"path": path, "offset": mid_at, "length": 1000})).await;
        assert!(mid.ok, "{}", mid.error);
        assert_eq!(decode(&mid), &data[mid_at..mid_at + 1000]);
        assert_eq!(mid.result["eof"], json!(false));

        let end_at = data.len() - 100;
        let end = download_part(&r, json!({"path": path, "offset": end_at})).await;
        assert!(end.ok, "{}", end.error);
        assert_eq!(decode(&end), &data[end_at..]);
        assert_eq!(end.result["length"], json!(100));
        assert_eq!(end.result["eof"], json!(true));
        assert_eq!(end.result["sha256"], json!(hex(&Sha256::digest(&data))));

        let at_end = download_part(&r, json!({"path": path, "offset": size})).await;
        assert!(at_end.ok, "{}", at_end.error);
        assert_eq!(at_end.result["length"], json!(0));
        assert_eq!(at_end.result["eof"], json!(true));

        for out in [&first, &mid, &end, &at_end] {
            assert_eq!(out.result["size"], json!(size));
            assert_eq!(out.result["mtime"], json!(mtime));
            assert_eq!(out.result["path"], json!(path.to_string_lossy()));
        }

        let mut joined = Vec::new();
        let mut offset = 0u64;
        loop {
            let part = download_part(&r, json!({"path": path, "offset": offset})).await;
            assert!(part.ok, "{}", part.error);
            joined.extend(decode(&part));
            offset += part.result["length"].as_u64().unwrap();
            if part.result["eof"] == json!(true) {
                break;
            }
        }
        assert_eq!(joined, data);
    }

    #[tokio::test]
    async fn download_part_refuses_an_offset_past_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        fs::write(&path, "hello").unwrap();
        let out = download_part(&download_router(), json!({"path": path, "offset": 6})).await;
        assert!(!out.ok);
        assert_eq!(
            out.error,
            format!(
                "fs.download: offset 6 is past the end of {}, which is 5 bytes",
                path.to_string_lossy()
            )
        );
    }

    #[tokio::test]
    async fn download_part_length_is_capped_at_four_mib() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        fs::write(&path, patterned(5 * 1024 * 1024)).unwrap();
        let out = download_part(
            &download_router(),
            json!({"path": path, "length": 9 * 1024 * 1024}),
        )
        .await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["length"], json!(CHUNK_CAP));
        assert_eq!(out.result["offset"], json!(0));
        assert_eq!(out.clamped.len(), 1);
        assert_eq!(out.clamped[0].to, CHUNK_CAP as i64);

        let direct = fs_download(args(json!({"path": path, "length": 9 * 1024 * 1024}))).unwrap();
        assert_eq!(direct["length"], json!(CHUNK_CAP));
        let bad = fs_download(args(json!({"path": path, "length": 0}))).unwrap_err();
        assert_eq!(bad.message, "fs.download: 'length' must be 1 or more");
    }

    #[tokio::test]
    async fn download_part_refuses_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        let key = ssh.join("id_ed25519");
        fs::write(&key, "PRIVATE").unwrap();
        let out = download_part(
            &download_router(),
            json!({"path": key, "offset": 0, "length": 4}),
        )
        .await;
        assert!(!out.ok);
        assert!(
            out.error.ends_with("because it can hold passwords or keys"),
            "{}",
            out.error
        );
    }

    #[tokio::test]
    async fn download_without_offset_or_length_is_one_reply_up_to_8_mib() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("b.txt");
        fs::write(&small, "hi").unwrap();
        let r = download_router();
        let out = download_part(&r, json!({"path": small})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["base64"], json!("aGk="));
        assert_eq!(
            out.result["sha256"],
            json!("8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4")
        );
        assert_eq!(out.result["size"], json!(2));
        let mut keys: Vec<&String> = out.result.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["base64", "mtime", "path", "sha256", "size"]);

        let big = dir.path().join("big.bin");
        fs::write(&big, patterned(8 * 1024 * 1024 + 1)).unwrap();
        let out = download_part(&r, json!({"path": big})).await;
        assert!(!out.ok);
        assert_eq!(
            out.error,
            format!(
                "fs.download: {} is 8.0 MiB, over the 8 MiB limit. Download it in parts with offset and length.",
                big.to_string_lossy()
            )
        );
    }
}

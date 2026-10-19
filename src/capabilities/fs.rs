use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::dispatch::{handler, Args, CapResult, HandlerError, Router};

const READ_CAP: u64 = 8 * 1024 * 1024;
const TEXT_SAMPLE_CAP: u64 = 256 * 1024;

pub fn register(r: &mut Router) {
    r.register("fs.ls", blocking_handler(fs_ls));
    r.register("fs.read", blocking_handler(fs_read));
    r.register("fs.search", blocking_handler(fs_search));
    r.register("fs.head", blocking_handler(|a| fs_text_sample(a, false)));
    r.register("fs.tail", blocking_handler(|a| fs_text_sample(a, true)));
    r.register("fs.stat", blocking_handler(fs_stat));
    r.register("fs.tree", blocking_handler(fs_tree));
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

pub(crate) fn go_is_abs(path: &str) -> bool {
    let l = volume_len(path);
    if l == 0 {
        return false;
    }
    let b = path.as_bytes();
    if is_slash(b[0]) && is_slash(b[1]) {
        return true;
    }
    let rest = &path[l..];
    !rest.is_empty() && is_slash(rest.as_bytes()[0])
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
    let path = str_arg(&args, "path").unwrap_or_default();
    if path.is_empty() {
        return Err(err("fs.ls: 'path' is required"));
    }
    let path = go_clean(&path);

    let rd = fs::read_dir(&path).map_err(|e| {
        if Path::new(&path).is_file() {
            path_err("open", &path, &io::Error::from_raw_os_error(3))
        } else {
            let cause = fs::metadata(&path).err().unwrap_or(e);
            path_err("open", &path, &cause)
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

pub(crate) fn fs_read(args: Args) -> CapResult {
    let path = str_arg(&args, "path").unwrap_or_default();
    if path.is_empty() {
        return Err(err("fs.read: 'path' is required"));
    }
    let path = go_clean(&path);

    let lp = path.replace('\\', "/").to_lowercase();
    for bad in [
        "/windows/system32/config/",
        "/windows/system32/configstore/",
        "/users/all users/microsoft/crypto/",
    ] {
        if lp.contains(bad) {
            return Err(err(format!(
                "fs.read: refusing to read {bad} (sensitive system path)"
            )));
        }
    }

    let info = stat(&path).map_err(|e| path_err("GetFileAttributesEx", &path, &e))?;
    if info.dir {
        return Err(err(format!("fs.read: {path} is a directory; use fs.ls")));
    }
    if info.size > READ_CAP {
        return Err(err(format!(
            "fs.read: file is {} bytes; cap is {}",
            info.size, READ_CAP
        )));
    }

    let mut f = fs::File::open(&path).map_err(|e| path_err("open", &path, &e))?;
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

fn fs_search(args: Args) -> CapResult {
    let search_path = str_arg(&args, "path").unwrap_or_default();
    if search_path.is_empty() {
        return Err(err("fs.search: 'path' is required"));
    }
    let search_path = go_clean(&search_path);
    if !go_is_abs(&search_path) {
        return Err(err("fs.search: path must be absolute"));
    }

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

    let root = stat(&search_path).map_err(|e| path_err("GetFileAttributesEx", &search_path, &e))?;
    if !root.dir {
        return Err(err(format!(
            "fs.search: {search_path:?} is not a directory"
        )));
    }

    let lower_pattern = pattern.to_lowercase();
    let root_depth = slash_depth(&go_clean(&search_path));
    let mut entries: Vec<Value> = Vec::new();

    walk_dir(&search_path, &mut |node| {
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
    Ok(json!({
        "path": search_path,
        "pattern": pattern,
        "max_depth": max_depth,
        "max_results": max_results,
        "include_dirs": include_dirs,
        "include_hidden": include_hidden,
        "count": count,
        "entries": entries,
    }))
}

fn fs_text_sample(args: Args, tail: bool) -> CapResult {
    let mode = if tail { "tail" } else { "head" };
    let path = str_arg(&args, "path").unwrap_or_default();
    let mut lines = int_arg(&args, "lines").unwrap_or(80);
    let path = path.trim().to_string();
    if path.is_empty() {
        return Err(err(format!("fs.{mode}: 'path' is required")));
    }
    let path = go_clean(&path);
    if !go_is_abs(&path) {
        return Err(err(format!("fs.{mode}: path must be absolute")));
    }
    if lines <= 0 {
        lines = 80;
    }
    if lines > 5000 {
        lines = 5000;
    }

    let info = stat(&path).map_err(|e| path_err("GetFileAttributesEx", &path, &e))?;
    if info.dir {
        return Err(err(format!("fs.{mode}: {path:?} is a directory")));
    }

    let mut f = fs::File::open(&path).map_err(|e| path_err("open", &path, &e))?;
    let mut data: Vec<u8> = Vec::new();
    if tail {
        let read_size = TEXT_SAMPLE_CAP.min(info.size);
        f.seek(SeekFrom::End(-(read_size as i64)))
            .map_err(|e| path_err("seek", &path, &e))?;
        f.by_ref()
            .take(read_size)
            .read_to_end(&mut data)
            .map_err(|e| path_err("read", &path, &e))?;
        if info.size > read_size {
            if let Some(i) = data.iter().position(|b| *b == b'\n') {
                if i + 1 < data.len() {
                    data.drain(..=i);
                }
            }
        }
    } else {
        f.by_ref()
            .take(TEXT_SAMPLE_CAP)
            .read_to_end(&mut data)
            .map_err(|e| path_err("read", &path, &e))?;
    }

    let text = go_lossy(&data);
    let mut all: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    let lines = lines as usize;
    if tail {
        if all.len() > lines {
            all = all.split_off(all.len() - lines);
        }
    } else if all.len() > lines {
        all.truncate(lines);
    }

    Ok(json!({
        "path": path,
        "size": info.size,
        "mtime": fmt_time(info.mtime),
        "lines": all,
        "line_count": all.len(),
        "truncated": info.size > data.len() as u64,
        "mode": mode,
    }))
}

fn fs_stat(args: Args) -> CapResult {
    let path = str_arg(&args, "path").unwrap_or_default();
    let path = path.trim().to_string();
    if path.is_empty() {
        return Err(err("fs.stat: 'path' is required"));
    }
    let path = go_clean(&path);
    if !go_is_abs(&path) {
        return Err(err("fs.stat: path must be absolute"));
    }

    let info = lstat(&path).map_err(|e| path_err("GetFileAttributesEx", &path, &e))?;
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
    let path = str_arg(&args, "path").unwrap_or_default();
    let path = path.trim().to_string();
    if path.is_empty() {
        return Err(err("fs.tree: 'path' is required"));
    }
    let path = go_clean(&path);
    if !go_is_abs(&path) {
        return Err(err("fs.tree: path must be absolute"));
    }

    let max_depth = int_arg(&args, "max_depth").unwrap_or(3).clamp(1, 8);
    let max_entries = int_arg(&args, "max_entries").unwrap_or(500).clamp(50, 5000);

    let root = stat(&path).map_err(|e| path_err("GetFileAttributesEx", &path, &e))?;
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
        assert!(go_is_abs("C:\\x"));
        assert!(!go_is_abs("\\x"));
        assert!(!go_is_abs("C:x"));
        assert!(!go_is_abs("relative"));
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
        assert_eq!(out.result["truncated"], json!(true));
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
        assert!(dir_read.error.ends_with("is a directory; use fs.ls"));

        let sensitive = run(
            "fs.read",
            json!({"path": "C:\\Windows\\System32\\config\\SAM"}),
        )
        .await;
        assert!(sensitive.error.contains("sensitive system path"));
    }
}

use super::fs::err;
use super::fs::int_arg;
use super::fs::path_err;
use super::fs::read_err;
use super::fs::str_arg;
use super::fs_meta::fmt_time;
use super::fs_meta::info_of;
use super::fs_meta::lstat;
use super::fs_meta::stat;
use super::fs_path::go_base;
use super::fs_path::go_clean;
use super::fs_path::go_join;
use super::fs_path::local_path;
use super::fs_walk::Flow;
use super::fs_walk::slash_depth;
use super::fs_walk::walk_dir;
use crate::dispatch::{Args, CapResult};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::Path;

pub(super) const DU_ENTRY_CAP: i64 = 1_000_000;

pub(super) fn fs_ls(args: &Args) -> CapResult {
    let path = local_path("fs.ls", &str_arg(args, "path").unwrap_or_default())?;

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

pub(super) fn fs_stat(args: &Args) -> CapResult {
    let path = local_path("fs.stat", &str_arg(args, "path").unwrap_or_default())?;

    let info = lstat(&path).map_err(|e| read_err("fs.stat", "GetFileAttributesEx", &path, &e))?;
    let mut target = String::new();
    if info.symlink
        && let Ok(dest) = fs::read_link(&path)
    {
        target = dest.to_string_lossy().into_owned();
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

pub(super) fn fs_tree(args: &Args) -> CapResult {
    let path = local_path("fs.tree", &str_arg(args, "path").unwrap_or_default())?;

    let max_depth = int_arg(args, "max_depth").unwrap_or(3).clamp(1, 8);
    let max_entries = int_arg(args, "max_entries").unwrap_or(500).clamp(50, 5000);

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
        if crate::util::count_i64(entries.len()) >= max_entries {
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
        "truncated": crate::util::count_i64(count) >= max_entries,
        "entries": entries,
    }))
}

#[derive(Default)]
struct DuChild {
    dir: bool,
    bytes: u64,
    files: u64,
}

pub(super) fn fs_du(args: &Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
    let path = local_path("fs.du", &str_arg(args, "path").unwrap_or_default())?;
    let top = usize::try_from(int_arg(args, "top").unwrap_or(20).clamp(1, 200)).unwrap_or(20);
    let max_entries = int_arg(args, "max_entries")
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

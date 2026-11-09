use super::fs::bool_arg;
use super::fs::err;
use super::fs::int_arg;
use super::fs::read_err;
use super::fs::str_arg;
use super::fs_glob::glob_match;
use super::fs_meta::fmt_time;
use super::fs_meta::stat;
use super::fs_path::go_clean;
use super::fs_path::local_path;
use super::fs_walk::Flow;
use super::fs_walk::slash_depth;
use super::fs_walk::walk_dir;
use crate::dispatch::{Args, CapResult};
use serde_json::{Value, json};

pub(super) const SEARCH_VISIT_CAP: u64 = 200_000;

fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.') && name.len() > 1
}

pub(super) fn fs_search(args: &Args, cancel: &tokio_util::sync::CancellationToken) -> CapResult {
    search(args, cancel, SEARCH_VISIT_CAP)
}

pub(super) fn search(
    args: &Args,
    cancel: &tokio_util::sync::CancellationToken,
    visit_cap: u64,
) -> CapResult {
    let search_path = local_path("fs.search", &str_arg(args, "path").unwrap_or_default())?;

    let mut pattern = str_arg(args, "pattern").unwrap_or_else(|| "*".to_string());
    if pattern.is_empty() {
        pattern = "*".to_string();
    }
    let pattern = pattern.trim().to_string();
    if let Err(e) = glob_match(&pattern, "probe.txt") {
        return Err(err(format!("fs.search: bad pattern: {e}")));
    }

    let mut max_results = int_arg(args, "max_results").unwrap_or(200);
    if max_results <= 0 {
        max_results = 200;
    }
    if max_results > 2000 {
        max_results = 2000;
    }
    let mut max_depth = int_arg(args, "max_depth").unwrap_or(4);
    max_depth = max_depth.clamp(0, 20);
    let include_dirs = bool_arg(args, "include_dirs").unwrap_or(false);
    let include_hidden = bool_arg(args, "include_hidden").unwrap_or(false);

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
        if crate::util::count_i64(entries.len()) >= max_results {
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
        "truncated": visit_cap_hit || crate::util::count_i64(count) >= max_results,
        "entries": entries,
    });
    if visit_cap_hit {
        out["note"] = json!(format!(
            "stopped after looking at {visit_cap} files and folders; search a smaller folder or lower max_depth"
        ));
    }
    Ok(out)
}

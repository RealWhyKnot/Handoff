use super::fs_meta::Info;
use super::fs_meta::info_of;
use super::fs_path::go_join;
use super::fs_path::os_basename;
use std::fs;
use std::path::Path;

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

pub(super) fn slash_depth(path: &str) -> i64 {
    crate::util::count_i64(path.replace('\\', "/").split('/').count())
}

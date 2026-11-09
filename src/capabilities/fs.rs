use super::fs_dir::fs_du;
use super::fs_dir::fs_ls;
use super::fs_dir::fs_stat;
use super::fs_dir::fs_tree;
use super::fs_read::fs_read;
use super::fs_search::fs_search;
use std::fmt::Write as _;
use std::io::{self};

use serde_json::Value;

use crate::dispatch::{Args, CapResult, HandlerError, Router, handler};

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
                    run_blocking(move || fs_read(&args, &cancel)).await
                }),
            ),
            (
                "fs.search",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || fs_search(&args, &cancel)).await
                }),
            ),
            (
                "fs.head",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || super::fs_lines_range::head(&args, &cancel)).await
                }),
            ),
            (
                "fs.tail",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || super::fs_lines_tail::tail(&args, &cancel)).await
                }),
            ),
            (
                "fs.grep",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || super::fs_lines_grep::grep(&args, &cancel)).await
                }),
            ),
            ("fs.stat", blocking_handler(fs_stat)),
            ("fs.tree", blocking_handler(fs_tree)),
            (
                "fs.du",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || fs_du(&args, &cancel)).await
                }),
            ),
        ],
    );
}

pub(crate) fn blocking_handler<F>(f: F) -> crate::dispatch::Handler
where
    F: Fn(&Args) -> CapResult + Send + Sync + Clone + 'static,
{
    handler(move |_ctx, args| {
        let f = f.clone();
        async move { run_blocking(move || f(&args)).await }
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

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::fs_dir::DU_ENTRY_CAP;
    use crate::capabilities::fs_glob::glob_match;
    use crate::capabilities::fs_guard::is_secret_path;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    use crate::capabilities::fs_meta::fmt_time;
    use crate::capabilities::fs_path::go_base;
    use crate::capabilities::fs_path::go_clean;
    use crate::capabilities::fs_path::local_path;
    use crate::capabilities::fs_read::CHUNK_CAP;
    use crate::capabilities::fs_read::fs_download;
    use crate::capabilities::fs_search::SEARCH_VISIT_CAP;
    use crate::capabilities::fs_search::search;
    use crate::dispatch::Ctx;
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use std::time::UNIX_EPOCH;

    fn args(v: Value) -> Args {
        serde_json::from_value(v).unwrap_or_default()
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
            writeln!(s, "line-{i}").unwrap();
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
            writeln!(s, "entry-{i:06}").unwrap();
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
        let out = search(&a, &cancel, 10).unwrap();
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(out["visited"], json!(10));
        assert_eq!(out["count"], json!(9));
        assert_eq!(
            out["note"],
            json!(
                "stopped after looking at 10 files and folders; search a smaller folder or lower max_depth"
            )
        );
        let full = search(&a, &cancel, SEARCH_VISIT_CAP).unwrap();
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
        (0..len)
            .map(|i| u8::try_from(i * 7 % 251).unwrap())
            .collect()
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
        let size = crate::util::count_u64(data.len());
        let mtime = fmt_time(fs::metadata(&path).unwrap().modified().unwrap());
        let r = download_router();

        let first = download_part(&r, json!({"path": path, "offset": 0})).await;
        assert!(first.ok, "{}", first.error);
        assert_eq!(decode(&first), &data[..crate::util::size_usize(CHUNK_CAP)]);
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
        assert_eq!(out.clamped[0].to, i64::try_from(CHUNK_CAP).unwrap());

        let direct = fs_download(&args(json!({"path": path, "length": 9 * 1024 * 1024}))).unwrap();
        assert_eq!(direct["length"], json!(CHUNK_CAP));
        let bad = fs_download(&args(json!({"path": path, "length": 0}))).unwrap_err();
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

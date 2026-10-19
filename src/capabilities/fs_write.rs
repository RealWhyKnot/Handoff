use std::fs;
use std::io;
use std::path::Path;

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::dispatch::{handler, CapResult, HandlerError, Router};

use super::consent::require_risk_consent;
use super::fs::{
    blocking_handler, bool_arg, err, fmt_time, go_clean, go_is_abs, hex, lstat, path_err,
    run_blocking, stat, str_arg, volume_len,
};

pub fn register(r: &mut Router) {
    r.register(
        "fs.upload",
        handler(|ctx, args| async move {
            let prep = run_blocking(move || upload_prepare(&args)).await?;
            require_risk_consent(
                &ctx.cancel,
                "fs.upload",
                "Writes operator-supplied file content to this computer. Uploaded content can replace files when overwrite=true.",
            )
            .await?;
            run_blocking(move || upload_commit(prep)).await
        }),
    );
    r.register("fs.download", blocking_handler(super::fs::fs_read));
    r.register(
        "fs.mkdir",
        handler(|ctx, args| async move {
            let path = str_arg(&args, "path").unwrap_or_default();
            if path.is_empty() {
                return Err(err("fs.mkdir: 'path' is required"));
            }
            let path = go_clean(&path);
            guard_write_path(&path)?;
            require_risk_consent(
                &ctx.cancel,
                "fs.mkdir",
                "Creates a directory on this computer. This changes the host filesystem.",
            )
            .await?;
            run_blocking(move || mkdir(&path)).await
        }),
    );
    r.register(
        "fs.delete",
        handler(|ctx, args| async move {
            let path = str_arg(&args, "path").unwrap_or_default();
            let recursive = bool_arg(&args, "recursive").unwrap_or(false);
            if path.is_empty() {
                return Err(err("fs.delete: 'path' is required"));
            }
            let path = go_clean(&path);
            guard_delete_path(&path)?;
            require_risk_consent(
                &ctx.cancel,
                "fs.delete",
                "Deletes a file or directory from this computer. Directory deletion requires recursive=true and cannot be undone by Handoff.",
            )
            .await?;
            run_blocking(move || delete(&path, recursive)).await
        }),
    );
}

struct Upload {
    path: String,
    data: Vec<u8>,
    sha: String,
    overwrite: bool,
}

fn upload_prepare(args: &crate::dispatch::Args) -> Result<Upload, HandlerError> {
    let path = str_arg(args, "path").unwrap_or_default();
    let data_b64 = str_arg(args, "data_base64").unwrap_or_default();
    let expect_sha = str_arg(args, "sha256").unwrap_or_default();
    let overwrite = bool_arg(args, "overwrite").unwrap_or(false);
    if path.is_empty() {
        return Err(err("fs.upload: 'path' is required"));
    }
    if data_b64.is_empty() {
        return Err(err("fs.upload: 'data_base64' is required"));
    }
    let path = go_clean(&path);

    guard_write_path(&path)?;

    let data = go_base64_decode(&data_b64).map_err(|off| {
        err(format!(
            "fs.upload: base64 decode: illegal base64 data at input byte {off}"
        ))
    })?;

    let got = hex(&Sha256::digest(&data));
    if !expect_sha.is_empty() && !expect_sha.eq_ignore_ascii_case(&got) {
        return Err(err(format!(
            "fs.upload: sha256 mismatch (got {got}, expected {expect_sha})"
        )));
    }
    Ok(Upload {
        path,
        data,
        sha: got,
        overwrite,
    })
}

fn upload_commit(up: Upload) -> CapResult {
    let path = up.path;
    if !up.overwrite && fs::metadata(&path).is_ok() {
        return Err(err(format!(
            "fs.upload: {path} already exists (pass overwrite=true to replace)"
        )));
    }
    if let Some(parent) = Path::new(&path).parent() {
        let parent = parent.to_string_lossy().into_owned();
        if !parent.is_empty() {
            mkdir_all(&parent)?;
        }
    }
    fs::write(&path, &up.data).map_err(|e| path_err("open", &path, &e))?;
    Ok(json!({
        "path": path,
        "size": up.data.len(),
        "sha256": up.sha,
    }))
}

fn mkdir(path: &str) -> CapResult {
    mkdir_all(path)?;
    let info = stat(path).map_err(|e| path_err("GetFileAttributesEx", path, &e))?;
    Ok(json!({
        "path": path,
        "dir": info.dir,
        "mtime": fmt_time(info.mtime),
    }))
}

pub(crate) fn mkdir_all(path: &str) -> Result<(), HandlerError> {
    if let Ok(info) = stat(path) {
        if info.dir {
            return Ok(());
        }
        return Err(HandlerError::plain(format!(
            "mkdir {path}: not a directory"
        )));
    }
    let b = path.as_bytes();
    let mut i = b.len();
    while i > 0 && (b[i - 1] == b'\\' || b[i - 1] == b'/') {
        i -= 1;
    }
    let mut j = i;
    while j > 0 && !(b[j - 1] == b'\\' || b[j - 1] == b'/') {
        j -= 1;
    }
    if j > 1 {
        let mut parent = path[..j - 1].to_string();
        if parent.len() == volume_len(&parent) && parent.len() == 2 {
            parent.push('\\');
        }
        mkdir_all(&parent)?;
    }
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Ok(info) = lstat(path) {
                if info.dir {
                    return Ok(());
                }
            }
            Err(path_err("mkdir", path, &e))
        }
    }
}

fn delete(path: &str, recursive: bool) -> CapResult {
    let info = lstat(path).map_err(|e| path_err("GetFileAttributesEx", path, &e))?;
    if info.dir && !recursive {
        return Err(err(format!(
            "fs.delete: {path} is a directory (pass recursive=true to remove it)"
        )));
    }
    let res: io::Result<()> = if info.dir {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path).or_else(|e| fs::remove_dir(path).map_err(|_| e))
    };
    res.map_err(|e| path_err("remove", path, &e))?;
    Ok(json!({
        "path": path,
        "dir": info.dir,
        "size": info.size,
        "recursive": recursive,
        "deleted": true,
    }))
}

pub(crate) fn guard_write_path(p: &str) -> Result<(), HandlerError> {
    guard_mutable_path("fs.upload", p)
}

pub(crate) fn guard_delete_path(p: &str) -> Result<(), HandlerError> {
    if !go_is_abs(p) {
        return Err(err("fs.delete: path must be absolute"));
    }
    if is_protected_root(p) {
        return Err(err(format!(
            "fs.delete: refusing to delete protected root {p}"
        )));
    }
    guard_mutable_path("fs.delete", p)
}

fn guard_mutable_path(kind: &str, p: &str) -> Result<(), HandlerError> {
    let mut lp = go_clean(p).replace('\\', "/").to_lowercase();
    if let Some(rest) = lp.strip_prefix("c:") {
        lp = rest.to_string();
    }
    let lp = lp.trim_end_matches('/');
    for bad in [
        "/windows/system32",
        "/windows/syswow64",
        "/program files",
        "/program files (x86)",
    ] {
        if lp == bad || lp.starts_with(&format!("{bad}/")) {
            return Err(err(format!("{kind}: refusing to modify under {bad}")));
        }
    }
    Ok(())
}

fn is_protected_root(p: &str) -> bool {
    let clean = go_clean(p);
    let vol = volume_len(&clean);
    if vol > 0 {
        let rest = &clean[vol..];
        if rest.is_empty() || rest == "\\" {
            return true;
        }
    }
    let mut protected: Vec<String> = ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .collect();
    if let Ok(home) = std::env::var("USERPROFILE") {
        protected.push(home);
    }
    protected
        .iter()
        .filter(|r| !r.is_empty())
        .any(|r| go_clean(r).to_lowercase() == clean.to_lowercase())
}

pub(crate) fn go_base64_decode(src: &str) -> Result<Vec<u8>, usize> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let src = src.as_bytes();
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    let mut si = 0;
    while si < src.len() {
        let mut dbuf = [0u8; 4];
        let mut dlen = 4;
        let mut trailing: Option<usize> = None;
        let mut j = 0;
        while j < 4 {
            if si == src.len() {
                if j == 0 {
                    return Ok(out);
                }
                return Err(si - j);
            }
            let c = src[si];
            si += 1;
            if let Some(v) = val(c) {
                dbuf[j] = v;
                j += 1;
                continue;
            }
            if c == b'\n' || c == b'\r' {
                continue;
            }
            if c != b'=' {
                return Err(si - 1);
            }
            match j {
                0 | 1 => return Err(si - 1),
                2 => {
                    while si < src.len() && (src[si] == b'\n' || src[si] == b'\r') {
                        si += 1;
                    }
                    if si == src.len() {
                        return Err(src.len());
                    }
                    if src[si] != b'=' {
                        return Err(si - 1);
                    }
                    si += 1;
                }
                _ => {}
            }
            while si < src.len() && (src[si] == b'\n' || src[si] == b'\r') {
                si += 1;
            }
            if si < src.len() {
                trailing = Some(si);
            }
            dlen = j;
            break;
        }
        let v = (dbuf[0] as u32) << 18
            | (dbuf[1] as u32) << 12
            | (dbuf[2] as u32) << 6
            | dbuf[3] as u32;
        let bytes = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        out.extend_from_slice(&bytes[..dlen - 1]);
        if let Some(off) = trailing {
            return Err(off);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::consent::testing::SessionGuard;
    use super::*;
    use crate::consent::Prompt;
    use crate::dispatch::{Args, Ctx};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn router() -> Router {
        let mut r = Router::new();
        register(&mut r);
        r
    }

    fn prompt(allow: bool) -> (Prompt, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let p: Prompt = Arc::new(move |_, _| {
            let c = c.clone();
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(allow)
            })
        });
        (p, calls)
    }

    async fn run(kind: &str, v: Value) -> crate::dispatch::Outcome {
        router().dispatch(Ctx::default(), kind, args(v)).await
    }

    #[test]
    fn registers_the_mutating_fs_kinds() {
        assert_eq!(
            router().kinds(),
            vec!["fs.delete", "fs.download", "fs.mkdir", "fs.upload"]
        );
    }

    #[tokio::test]
    async fn delete_requires_consent_before_removing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("victim.txt");
        fs::write(&path, "keep").unwrap();
        let (p, calls) = prompt(false);
        let _guard = SessionGuard::with_prompt(p);
        let out = run("fs.delete", json!({"path": path})).await;
        assert!(!out.ok);
        assert_eq!(out.error, "the host declined this request");
        assert!(path.exists());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn delete_removes_file_after_consent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("victim.txt");
        fs::write(&path, "delete").unwrap();
        let (p, _) = prompt(true);
        let _guard = SessionGuard::with_prompt(p);
        let out = run("fs.delete", json!({"path": path})).await;
        assert!(out.ok, "{}", out.error);
        assert!(!path.exists());
        assert_eq!(out.result["deleted"], json!(true));
        assert_eq!(out.result["dir"], json!(false));
        assert_eq!(out.result["size"], json!(6));

        let sub = dir.path().join("sub");
        fs::create_dir_all(sub.join("inner")).unwrap();
        let refused = run("fs.delete", json!({"path": sub})).await;
        assert!(!refused.ok);
        assert!(
            refused.error.contains("pass recursive=true"),
            "{}",
            refused.error
        );
        let removed = run("fs.delete", json!({"path": sub, "recursive": true})).await;
        assert!(removed.ok, "{}", removed.error);
        assert!(!sub.exists());
    }

    #[test]
    fn delete_guard_rejects_protected_directories() {
        for path in [
            "C:\\Windows\\System32",
            "C:\\Windows\\System32\\drivers\\etc\\hosts",
            "C:\\Program Files",
            "C:\\Program Files\\App",
            "C:\\",
            "relative",
        ] {
            assert!(guard_delete_path(path).is_err(), "{path} was allowed");
        }
        assert!(guard_write_path("C:\\Windows\\SysWOW64\\x.dll").is_err());
        assert!(guard_write_path("C:\\Users\\someone\\x.txt").is_ok());
    }

    #[tokio::test]
    async fn consent_is_requested_before_probing_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.txt");
        let (p, calls) = prompt(false);
        let _guard = SessionGuard::with_prompt(p);
        let out = run("fs.delete", json!({"path": missing})).await;
        assert_eq!(out.error, "the host declined this request");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let (p, calls) = prompt(false);
        super::super::consent::set_session_ledger(Arc::new(crate::consent::Ledger::new(p)));
        let out = run("fs.upload", json!({"path": missing, "data_base64": "aGk="})).await;
        assert_eq!(out.error, "the host declined this request");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!missing.exists());
    }

    #[tokio::test]
    async fn upload_writes_checks_the_hash_and_refuses_to_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("er").join("file.bin");
        let (p, _) = prompt(true);
        let _guard = SessionGuard::with_prompt(p);
        let sha = "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4";

        let bad = run(
            "fs.upload",
            json!({"path": path, "data_base64": "aGk=", "sha256": "00"}),
        )
        .await;
        assert!(
            bad.error
                .starts_with("fs.upload: sha256 mismatch (got 8f43"),
            "{}",
            bad.error
        );
        assert!(!path.exists());

        let ok = run(
            "fs.upload",
            json!({"path": path, "data_base64": "aGk=", "sha256": sha.to_uppercase()}),
        )
        .await;
        assert!(ok.ok, "{}", ok.error);
        assert_eq!(ok.result["size"], json!(2));
        assert_eq!(ok.result["sha256"], json!(sha));
        assert_eq!(fs::read(&path).unwrap(), b"hi");

        let again = run("fs.upload", json!({"path": path, "data_base64": "eW8="})).await;
        assert!(again
            .error
            .contains("already exists (pass overwrite=true to replace)"));
        let over = run(
            "fs.upload",
            json!({"path": path, "data_base64": "eW8=", "overwrite": true}),
        )
        .await;
        assert!(over.ok, "{}", over.error);
        assert_eq!(fs::read(&path).unwrap(), b"yo");

        let missing = run("fs.upload", json!({"path": path})).await;
        assert_eq!(missing.error, "fs.upload: 'data_base64' is required");
        let garbled = run("fs.upload", json!({"path": path, "data_base64": "a$b="})).await;
        assert_eq!(
            garbled.error,
            "fs.upload: base64 decode: illegal base64 data at input byte 1"
        );
    }

    #[tokio::test]
    async fn mkdir_creates_nested_directories_and_download_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("b");
        let (p, _) = prompt(true);
        let _guard = SessionGuard::with_prompt(p);
        let out = run("fs.mkdir", json!({"path": target})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["dir"], json!(true));
        assert!(target.is_dir());
        let again = run("fs.mkdir", json!({"path": target})).await;
        assert!(again.ok);

        let file = target.join("f.txt");
        fs::write(&file, "hi").unwrap();
        let blocked = run("fs.mkdir", json!({"path": file})).await;
        assert!(!blocked.ok);
        assert!(
            blocked.error.ends_with(": not a directory"),
            "{}",
            blocked.error
        );

        let dl = run("fs.download", json!({"path": file})).await;
        assert!(dl.ok, "{}", dl.error);
        assert_eq!(dl.result["base64"], json!("aGk="));
    }

    #[test]
    fn base64_decoder_matches_go_offsets() {
        assert_eq!(go_base64_decode("aGk=").unwrap(), b"hi");
        assert_eq!(go_base64_decode("aG\nk=").unwrap(), b"hi");
        assert_eq!(go_base64_decode("aGk").unwrap_err(), 0);
        assert_eq!(go_base64_decode("aGk=x").unwrap_err(), 4);
        assert_eq!(go_base64_decode("a$b=").unwrap_err(), 1);
        assert_eq!(go_base64_decode("aA=").unwrap_err(), 3);
        assert_eq!(go_base64_decode("YQ==").unwrap(), b"a");
    }
}

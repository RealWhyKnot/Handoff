use std::fs;
use std::io;
use std::path::Path;

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::dispatch::{for_display, handler, CapResult, HandlerError, Router};

use super::consent::require_risk_consent;
use super::fs::{
    bool_arg, err, fmt_time, hex, local_path, lstat, path_err, run_blocking, stat, str_arg,
    volume_len,
};
pub(crate) use super::fs_guard::{guard_delete_path, guard_write_path};

pub fn register(r: &mut Router) {
    super::register_specs(
        r,
        include_str!("specs/fs_write.json"),
        vec![
            (
                "fs.upload",
                handler(|ctx, args| async move {
                    let prep = run_blocking(move || upload_prepare(&args)).await?;
                    let summary = upload_summary(&prep);
                    require_risk_consent(&ctx.cancel, "fs.upload", &summary).await?;
                    guard_write_path("fs.upload", &prep.path)?;
                    run_blocking(move || upload_commit(prep)).await
                }),
            ),
            (
                "fs.download",
                handler(|ctx, args| async move {
                    let cancel = ctx.cancel.clone();
                    run_blocking(move || super::fs::download(args, &cancel)).await
                }),
            ),
            (
                "fs.mkdir",
                handler(|ctx, args| async move {
                    let path = local_path("fs.mkdir", &str_arg(&args, "path").unwrap_or_default())?;
                    guard_write_path("fs.mkdir", &path)?;
                    let summary = format!(
                        "Creates the folder {} on this computer, along with any missing parent folders.",
                        shown_path(&path)
                    );
                    require_risk_consent(&ctx.cancel, "fs.mkdir", &summary).await?;
                    guard_write_path("fs.mkdir", &path)?;
                    run_blocking(move || mkdir(&path)).await
                }),
            ),
            (
                "fs.delete",
                handler(|ctx, args| async move {
                    let recursive = bool_arg(&args, "recursive").unwrap_or(false);
                    let path =
                        local_path("fs.delete", &str_arg(&args, "path").unwrap_or_default())?;
                    guard_delete_path(&path)?;
                    require_risk_consent(
                        &ctx.cancel,
                        "fs.delete",
                        &delete_summary(&path, recursive),
                    )
                    .await?;
                    guard_delete_path(&path)?;
                    run_blocking(move || delete(&path, recursive)).await
                }),
            ),
        ],
    );
}

pub(crate) fn shown_path(path: &str) -> String {
    let shown = for_display(path, 400, 1);
    if shown.cut {
        format!("{}...", shown.text)
    } else {
        shown.text
    }
}

fn upload_summary(up: &Upload) -> String {
    let mut text = format!(
        "Saves a file from your helper to {} on this computer ({} bytes).",
        shown_path(&up.path),
        up.data.len()
    );
    if up.overwrite {
        text.push_str(" If a file is already there, it will be replaced.");
    }
    text
}

fn delete_summary(path: &str, recursive: bool) -> String {
    let mut text = format!("Deletes {} from this computer.", shown_path(path));
    if recursive {
        text.push_str(" If this is a folder, everything inside it is deleted too.");
    }
    text.push_str(" Handoff can't undo this.");
    text
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
    let path = local_path("fs.upload", &path)?;
    if data_b64.is_empty() {
        return Err(err("fs.upload: 'data_base64' is required"));
    }

    guard_write_path("fs.upload", &path)?;

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
            "C:\\Program Files.\\App",
            "C:\\Program Files \\App",
            "D:\\Program Files\\App",
            "C:\\",
            "C:\\.",
            "relative",
        ] {
            assert!(guard_delete_path(path).is_err(), "{path} was allowed");
        }
        assert!(guard_write_path("fs.upload", "C:\\Windows\\SysWOW64\\x.dll").is_err());
        assert!(guard_write_path("fs.upload", "C:\\Users\\someone\\x.txt").is_ok());
        assert!(guard_write_path("fs.upload", "C:\\Program Files Extra\\x.txt").is_ok());
    }

    #[test]
    fn delete_guard_sees_the_profile_through_trailing_dots() {
        let home = std::env::var("USERPROFILE").unwrap();
        for path in [
            home.clone(),
            format!("{home}\\"),
            format!("{home}."),
            format!("{home}. \\"),
            home.to_uppercase(),
        ] {
            let out = guard_delete_path(&path);
            assert!(
                out.as_ref()
                    .is_err_and(|e| e.message.contains("won't delete")),
                "{path}: {out:?}"
            );
        }
    }

    #[test]
    fn write_guard_blocks_ssh_autostart_and_secrets() {
        for (path, why) in [
            ("C:\\Users\\bob\\.ssh\\authorized_keys", "SSH keys"),
            ("C:\\Users\\bob\\.ssh", "SSH keys"),
            ("C:\\Users\\bob\\.SSH.\\authorized_keys", "SSH keys"),
            ("C:\\Users\\bob\\.ssh \\config", "SSH keys"),
            ("C:\\Users\\bob\\.ssh\\new\\deeper\\authorized_keys", "SSH keys"),
            ("C:\\ProgramData\\ssh\\administrators_authorized_keys", "SSH keys"),
            ("C:\\ProgramData\\ssh\\sshd_config", "SSH keys"),
            ("C:\\ProgramData\\ssh", "SSH keys"),
            ("D:\\ProgramData\\SSH\\sshd_config", "SSH keys"),
            (
                "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Startup\\run.lnk",
                "Startup folder",
            ),
            (
                "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Startup",
                "Startup folder",
            ),
            (
                "C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\StartUp\\run.bat",
                "Startup folder",
            ),
            (
                "C:\\Users\\bob\\Documents\\WindowsPowerShell\\Microsoft.PowerShell_profile.ps1",
                "PowerShell runs",
            ),
            (
                "C:\\Users\\bob\\OneDrive\\Documents\\PowerShell\\profile.ps1",
                "PowerShell runs",
            ),
            (
                "C:\\Users\\bob\\Documents\\PowerShell\\Microsoft.VSCode_profile.ps1",
                "PowerShell runs",
            ),
            ("C:\\Users\\bob\\.aws\\credentials", "passwords or keys"),
            ("C:\\Users\\bob\\.git-credentials", "passwords or keys"),
            ("C:\\Users\\bob\\.docker\\config.json", "passwords or keys"),
            ("C:\\Users\\bob\\Documents\\vault.kdbx", "passwords or keys"),
        ] {
            for kind in ["fs.upload", "fs.mkdir", "fs.write"] {
                let out = guard_write_path(kind, path);
                let msg = out.as_ref().err().map(|e| e.message.clone()).unwrap_or_default();
                assert!(
                    msg.starts_with(&format!("{kind}: ")) && msg.contains(why),
                    "{kind} {path}: {out:?}"
                );
            }
            let startup_entry = why == "Startup folder" && !path.ends_with("Startup");
            assert_eq!(guard_delete_path(path).is_ok(), startup_entry, "delete {path}");
        }
        for path in [
            "C:\\Users\\bob\\Documents\\ssh-notes.txt",
            "C:\\Users\\bob\\sshkeys-backup\\readme.txt",
            "C:\\Users\\bob\\Documents\\PowerShell\\Scripts\\fix.ps1",
            "C:\\Users\\bob\\Documents\\profile.ps1",
            "C:\\Users\\bob\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\Tools\\x.lnk",
            "C:\\ProgramData\\sshd-backup\\notes.txt",
        ] {
            assert!(guard_write_path("fs.upload", path).is_ok(), "{path} was blocked");
            assert!(guard_delete_path(path).is_ok(), "delete {path} was blocked");
        }
    }

    fn write_router() -> Router {
        let mut r = router();
        super::super::fs_text::register(&mut r);
        r
    }

    fn write_attempts(path: &Path) -> Vec<(&'static str, Value)> {
        vec![
            (
                "fs.upload",
                json!({"path": path, "data_base64": "aGk=", "overwrite": true}),
            ),
            (
                "fs.write",
                json!({"path": path, "content": "ssh-ed25519 AAAA", "append": true}),
            ),
            ("fs.mkdir", json!({"path": path})),
            ("fs.delete", json!({"path": path, "recursive": true})),
        ]
    }

    #[tokio::test]
    async fn every_write_kind_refuses_ssh_before_asking() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        let keys = ssh.join("authorized_keys");
        fs::write(&keys, "original").unwrap();
        let (p, calls) = prompt(true);
        let _guard = SessionGuard::with_prompt(p);
        for target in [
            keys.clone(),
            ssh.join("sub").join("authorized_keys"),
            ssh.clone(),
        ] {
            for (kind, a) in write_attempts(&target) {
                let out = write_router().dispatch(Ctx::default(), kind, args(a)).await;
                assert!(!out.ok, "{kind} {target:?} went through");
                assert!(out.error.contains("SSH keys"), "{kind}: {}", out.error);
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(fs::read_to_string(&keys).unwrap(), "original");
        assert!(!ssh.join("sub").exists());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_junction_does_not_get_around_the_write_blocklist() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        fs::create_dir(&ssh).unwrap();
        let keys = ssh.join("authorized_keys");
        fs::write(&keys, "original").unwrap();
        let link = dir.path().join("innocent");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&ssh)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let (p, calls) = prompt(true);
        let _guard = SessionGuard::with_prompt(p);
        for target in [
            link.join("authorized_keys"),
            link.join("new").join("authorized_keys"),
            link.clone(),
        ] {
            for (kind, a) in write_attempts(&target) {
                let out = write_router().dispatch(Ctx::default(), kind, args(a)).await;
                assert!(!out.ok, "{kind} {target:?} followed the junction");
                assert!(out.error.contains("SSH keys"), "{kind}: {}", out.error);
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(fs::read_to_string(&keys).unwrap(), "original");
        assert!(!ssh.join("new").exists());
    }

    #[cfg(windows)]
    #[test]
    fn short_names_resolve_before_the_check() {
        let short = "C:\\PROGRA~1";
        if fs::metadata(short).is_err() {
            return;
        }
        let out = guard_write_path("fs.upload", &format!("{short}\\Evil\\x.dll"));
        assert!(
            out.as_ref()
                .is_err_and(|e| e.message.contains("installed programs need it")),
            "{out:?}"
        );
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

    fn recording_prompt() -> (Prompt, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        let p: Prompt = Arc::new(move |_, req| {
            s.lock().unwrap().push(req.summary.clone());
            Box::pin(async move { Ok(false) })
        });
        (p, seen)
    }

    #[tokio::test]
    async fn prompts_name_the_path_without_hidden_characters() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_string_lossy().into_owned();
        let sneaky = format!("{base}\\in\u{202E}voice\u{2066}.txt\u{7}");
        let clean = format!("{base}\\invoice.txt");
        let (p, seen) = recording_prompt();
        let _guard = SessionGuard::with_prompt(p);

        run(
            "fs.upload",
            json!({"path": sneaky, "data_base64": "aGk=", "overwrite": true}),
        )
        .await;
        super::super::consent::set_session_ledger(Arc::new(crate::consent::Ledger::new(
            recording_prompt_into(seen.clone()),
        )));
        run("fs.mkdir", json!({ "path": sneaky })).await;
        super::super::consent::set_session_ledger(Arc::new(crate::consent::Ledger::new(
            recording_prompt_into(seen.clone()),
        )));
        run("fs.delete", json!({"path": sneaky, "recursive": true})).await;

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                format!("Saves a file from your helper to {clean} on this computer (2 bytes). If a file is already there, it will be replaced."),
                format!("Creates the folder {clean} on this computer, along with any missing parent folders."),
                format!("Deletes {clean} from this computer. If this is a folder, everything inside it is deleted too. Handoff can't undo this."),
            ]
        );
        assert!(!Path::new(&clean).exists());
    }

    fn recording_prompt_into(seen: Arc<std::sync::Mutex<Vec<String>>>) -> Prompt {
        Arc::new(move |_, req| {
            seen.lock().unwrap().push(req.summary.clone());
            Box::pin(async move { Ok(false) })
        })
    }

    #[test]
    fn delete_prompt_without_recursive_is_shorter() {
        assert_eq!(
            delete_summary("C:\\temp\\a.txt", false),
            "Deletes C:\\temp\\a.txt from this computer. Handoff can't undo this."
        );
        let long = format!("C:\\{}", "d".repeat(1000));
        assert!(shown_path(&long).ends_with("..."));
        assert!(shown_path(&long).len() < 420);
    }

    #[tokio::test]
    async fn write_kinds_refuse_network_paths_before_asking() {
        let (p, calls) = prompt(true);
        let _guard = SessionGuard::with_prompt(p);
        for path in [
            "\\\\attacker\\share\\x",
            "\\\\?\\C:\\x",
            "\\\\.\\C:",
            "relative\\x",
        ] {
            for (kind, extra) in [
                ("fs.upload", json!({"data_base64": "aGk="})),
                ("fs.mkdir", json!({})),
                ("fs.delete", json!({})),
                ("fs.download", json!({})),
            ] {
                let mut a = extra.as_object().cloned().unwrap();
                a.insert("path".into(), json!(path));
                let out = router().dispatch(Ctx::default(), kind, a).await;
                assert!(!out.ok, "{kind} {path}");
                assert!(
                    out.error.contains("network and device paths")
                        || out.error.contains("must be absolute"),
                    "{kind} {path}: {}",
                    out.error
                );
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
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

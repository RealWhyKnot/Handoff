use std::fs;
use std::io::Write;

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::dispatch::{handler, Spec};
use crate::dispatch::{Args, CapResult, Router};

use super::consent::require_risk_consent;
use super::fs::{bool_arg, err, go_clean, go_is_abs, hex, path_err, run_blocking, str_arg};
use super::fs_write::{guard_write_path, mkdir_all};

const WRITE_CAP: usize = 1 << 20;

pub fn register(r: &mut Router) {
    let specs: Vec<Spec> = serde_json::from_str(include_str!("specs/fs_text.json"))
        .expect("embedded capability specs are valid JSON");
    for spec in specs {
        r.register_spec(
            spec,
            handler(|ctx, args| async move {
                let prep = fs_write_prepare(&args)?;
                require_risk_consent(
                    &ctx.cancel,
                    "fs.write",
                    &format!(
                        "Writes {} bytes of text to {} on this computer.",
                        prep.content_len, prep.path
                    ),
                )
                .await?;
                run_blocking(move || fs_write_commit(prep)).await
            }),
        );
    }
}

struct Prep {
    path: String,
    content: String,
    content_len: usize,
    encoding: String,
    newline: String,
    overwrite: bool,
    append: bool,
}

fn fs_write_prepare(args: &Args) -> Result<Prep, crate::dispatch::HandlerError> {
    let path = str_arg(args, "path").unwrap_or_default();
    let content = str_arg(args, "content").unwrap_or_default();
    let encoding = str_arg(args, "encoding").unwrap_or_else(|| "utf8".to_string());
    let newline = str_arg(args, "newline").unwrap_or_else(|| "crlf".to_string());
    let overwrite = bool_arg(args, "overwrite").unwrap_or(false);
    let append = bool_arg(args, "append").unwrap_or(false);

    let path = path.trim().to_string();
    if path.is_empty() {
        return Err(err("fs.write: 'path' is required"));
    }
    if !go_is_abs(&path) {
        return Err(err("fs.write: 'path' must be absolute"));
    }
    let path = go_clean(&path);
    if content.len() > WRITE_CAP {
        return Err(err(format!(
            "fs.write: content is {} bytes; cap is {}",
            content.len(),
            WRITE_CAP
        )));
    }
    guard_write_path(&path)?;
    Ok(Prep {
        path,
        content_len: content.len(),
        content,
        encoding,
        newline,
        overwrite,
        append,
    })
}

fn fs_write_commit(p: Prep) -> CapResult {
    let lf = p.content.replace("\r\n", "\n");
    let body = if p.newline == "crlf" {
        lf.replace('\n', "\r\n")
    } else {
        lf
    };

    let mut data = body.into_bytes();
    if p.encoding == "ascii" {
        if let Some(i) = data.iter().position(|b| *b > 0x7f) {
            return Err(err(format!("fs.write: content is not ascii at byte {i}")));
        }
    }

    let existed = fs::metadata(&p.path).is_ok();
    if existed && !p.overwrite && !p.append {
        return Err(err(format!(
            "fs.write: {} already exists (pass overwrite=true or append=true)",
            p.path
        )));
    }

    if p.encoding == "utf8bom" && !(p.append && existed) {
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.append(&mut data);
        data = with_bom;
    }

    if let Some(parent) = std::path::Path::new(&p.path).parent() {
        let parent = parent.to_string_lossy().into_owned();
        if !parent.is_empty() {
            mkdir_all(&parent)?;
        }
    }

    if p.append {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&p.path)
            .map_err(|e| path_err("open", &p.path, &e))?;
        f.write_all(&data)
            .map_err(|e| path_err("write", &p.path, &e))?;
    } else {
        fs::write(&p.path, &data).map_err(|e| path_err("open", &p.path, &e))?;
    }

    let info = fs::metadata(&p.path).map_err(|e| path_err("GetFileAttributesEx", &p.path, &e))?;
    Ok(json!({
        "path": p.path,
        "size": info.len(),
        "written": data.len(),
        "sha256": hex(&Sha256::digest(&data)),
        "created": !existed,
        "appended": p.append,
        "encoding": p.encoding,
        "newline": p.newline,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::consent::testing::SessionGuard;
    use super::*;
    use crate::consent::Prompt;
    use crate::dispatch::Ctx;
    use serde_json::Value;
    use std::sync::Arc;

    fn args(v: Value) -> Args {
        v.as_object().cloned().unwrap_or_default()
    }

    fn allow() -> Prompt {
        Arc::new(|_, _| Box::pin(async { Ok(true) }))
    }

    fn deny() -> Prompt {
        Arc::new(|_, _| Box::pin(async { Ok(false) }))
    }

    async fn run(v: Value) -> crate::dispatch::Outcome {
        let mut r = Router::new();
        register(&mut r);
        r.dispatch(Ctx::default(), "fs.write", args(v)).await
    }

    #[test]
    fn registers_fs_write_as_risky() {
        let mut r = Router::new();
        register(&mut r);
        assert_eq!(r.kinds(), vec!["fs.write"]);
        assert!(r.spec_for("fs.write").unwrap().risky);
    }

    #[tokio::test]
    async fn creates_and_refuses_to_clobber() {
        let _guard = SessionGuard::with_prompt(allow());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        let out =
            run(json!({"path": path, "content": "line one\nline two", "newline": "lf"})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(out.result["created"], json!(true));
        assert_eq!(fs::read_to_string(&path).unwrap(), "line one\nline two");

        let clobber = run(json!({"path": path, "content": "clobber"})).await;
        assert!(!clobber.ok);
        assert!(clobber.error.contains("pass overwrite=true or append=true"));

        let over = run(json!({"path": path, "content": "new", "overwrite": true})).await;
        assert!(over.ok, "{}", over.error);
        assert_eq!(over.result["created"], json!(false));
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[tokio::test]
    async fn honours_newline_append_and_bom() {
        let _guard = SessionGuard::with_prompt(allow());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.txt");
        let first = run(json!({"path": path, "content": "a\nb", "newline": "crlf"})).await;
        assert!(first.ok, "{}", first.error);
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb");

        let more =
            run(json!({"path": path, "content": "\nc", "newline": "crlf", "append": true})).await;
        assert!(more.ok, "{}", more.error);
        assert_eq!(more.result["appended"], json!(true));
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb\r\nc");

        let bom = dir.path().join("bom.txt");
        let out =
            run(json!({"path": bom, "content": "x", "encoding": "utf8bom", "newline": "lf"})).await;
        assert!(out.ok, "{}", out.error);
        assert_eq!(fs::read(&bom).unwrap(), b"\xEF\xBB\xBFx");
        assert_eq!(out.result["written"], json!(4));

        let appended = run(json!({"path": bom, "content": "y", "encoding": "utf8bom", "newline": "lf", "append": true})).await;
        assert!(appended.ok, "{}", appended.error);
        assert_eq!(fs::read(&bom).unwrap(), b"\xEF\xBB\xBFxy");

        let ascii = run(
            json!({"path": dir.path().join("a.txt"), "content": "caf\u{e9}", "encoding": "ascii"}),
        )
        .await;
        assert_eq!(ascii.error, "fs.write: content is not ascii at byte 3");
    }

    #[tokio::test]
    async fn rejects_relative_path_and_oversize_content() {
        let _guard = SessionGuard::with_prompt(allow());
        let rel = run(json!({"path": "relative.txt", "content": "x"})).await;
        assert!(!rel.ok);
        assert!(rel.error.contains("absolute"), "{}", rel.error);

        let dir = tempfile::tempdir().unwrap();
        let big = run(json!({
            "path": dir.path().join("big.txt"),
            "content": "x".repeat(WRITE_CAP + 1),
        }))
        .await;
        assert!(!big.ok);
        assert!(big.error.contains("cap is"), "{}", big.error);
    }

    #[tokio::test]
    async fn asks_before_touching_the_filesystem() {
        let _guard = SessionGuard::with_prompt(deny());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("denied.txt");
        let out = run(json!({"path": path, "content": "nope"})).await;
        assert!(!out.ok);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn read_only_session_refuses_writes() {
        let _guard = SessionGuard::new();
        super::super::consent::deny_all_risky();
        super::super::register_all(&mut Router::new(), None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("should-not-exist.txt");
        let out = run(json!({"path": path, "content": "nope"})).await;
        assert!(!out.ok);
        assert!(!path.exists());
    }
}

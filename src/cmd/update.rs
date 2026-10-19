use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::flags::{parse_options, Def, FlagError};
use super::version::{compare_versions, parse_version};
use crate::util;

const UPDATE_FLAGS: [Def; 4] = [
    Def::boolean("check", "report what is available without downloading").alias_of("check"),
    Def::boolean("c", "report what is available without downloading").alias_of("check"),
    Def::boolean("force", "update even from an unversioned local build"),
    Def::boolean(
        "no-replace",
        "download only, leaving the current binary in place",
    ),
];

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VersionManifest {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub url: String,
}

pub fn get_current_version() -> String {
    crate::VERSION.to_string()
}

pub fn should_update(current: &str, latest: &str, force: bool) -> bool {
    let cur = parse_version(current);
    let Some(next) = parse_version(latest) else {
        println!("the relay published an unreadable version; not updating");
        return false;
    };
    let Some(cur) = cur else {
        if !force {
            println!("this is a development build; not updating (use --force)");
            return false;
        }
        return true;
    };

    match compare_versions(&cur, &next) {
        0 => {
            println!("up to date");
            false
        }
        1 => {
            println!("already newer than the published build ({current} > {latest})");
            false
        }
        _ => true,
    }
}

pub fn replace_executable(exe_path: &Path, staged: &Path) -> Result<(), String> {
    let mut old = exe_path.as_os_str().to_owned();
    old.push(".old");
    let old = std::path::PathBuf::from(old);
    let _ = std::fs::remove_file(&old);

    if let Err(err) = std::fs::rename(exe_path, &old) {
        return Err(format!("move the current binary aside: {err}"));
    }
    if let Err(err) = std::fs::rename(staged, exe_path) {
        if let Err(restore) = std::fs::rename(&old, exe_path) {
            return Err(format!(
                "install failed ({err}) and the original could not be restored from {}: {restore}",
                old.display()
            ));
        }
        return Err(format!("install the new binary: {err}"));
    }
    let _ = std::fs::remove_file(&old);
    Ok(())
}

pub fn sweep_stale_update() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut old = exe.into_os_string();
    old.push(".old");
    let _ = std::fs::remove_file(old);
}

fn client() -> Result<reqwest::Client, String> {
    util::install_crypto_provider();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

pub async fn fetch_manifest(url: &str) -> Result<VersionManifest, String> {
    let resp = client()?.get(url).send().await.map_err(|e| e.to_string())?;
    if resp.status().as_u16() != 200 {
        return Err(format!("manifest fetch: {}", resp.status().as_u16()));
    }
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    let manifest: VersionManifest = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    if manifest.version.is_empty() {
        return Err("manifest has empty version".to_string());
    }
    Ok(manifest)
}

pub async fn download_verified(url: &str, sha256_hex: &str, dst: &Path) -> Result<(), String> {
    let mut resp = client()?.get(url).send().await.map_err(|e| e.to_string())?;
    if resp.status().as_u16() != 200 {
        return Err(resp.status().as_u16().to_string());
    }

    let mut file = std::fs::File::create(dst).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        hasher.update(&chunk);
        written += chunk.len() as u64;
    }
    file.flush().map_err(|e| e.to_string())?;
    drop(file);

    if written == 0 {
        return Err("empty body".to_string());
    }
    if !sha256_hex.is_empty() {
        let got = hex(&hasher.finalize());
        if !got.eq_ignore_ascii_case(sha256_hex) {
            return Err(format!(
                "sha256 mismatch (got {got}, expected {sha256_hex})"
            ));
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn update(args: &[String]) -> i32 {
    let (opts, parsed) = match parse_options("update", args, &UPDATE_FLAGS) {
        Ok(v) => v,
        Err(FlagError::Help) => return 0,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    let check = parsed.boolean("check");
    let force = parsed.boolean("force");
    let no_replace = parsed.boolean("no-replace");

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    runtime.block_on(run_update(&opts.relay, check, force, no_replace))
}

async fn run_update(relay: &str, check: bool, force: bool, no_replace: bool) -> i32 {
    let manifest_url = format!("{relay}/dl/handoff-version.json");
    let exe_url = format!("{relay}/dl/handoff.exe");

    let current = get_current_version();
    println!("current: {current}");
    println!("checking: {manifest_url}");

    let man = match fetch_manifest(&manifest_url).await {
        Ok(m) => m,
        Err(err) => {
            eprintln!("could not reach update manifest: {err}");
            return 1;
        }
    };
    println!("latest:  {}", man.version);
    if !man.notes.is_empty() {
        println!("notes:   {}", man.notes);
    }

    if !should_update(&current, &man.version, force) {
        return 0;
    }
    if check {
        println!("(check-only; not downloading)");
        return 0;
    }

    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(err) => {
            eprintln!("could not resolve current executable: {err}");
            return 1;
        }
    };
    let dst = exe_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("handoff.exe.new");

    println!("downloading: {exe_url} -> {}", dst.display());
    if let Err(err) = download_verified(&exe_url, &man.sha256, &dst).await {
        eprintln!("download failed: {err}");
        let _ = std::fs::remove_file(&dst);
        return 1;
    }
    println!("downloaded.");

    if no_replace {
        println!();
        println!("To finish the update:");
        println!("  1) Close any running 'handoff' processes.");
        println!("  2) Replace {}", exe_path.display());
        println!("     with     {}", dst.display());
        println!("  3) Start a new session.");
        return 0;
    }

    if let Err(err) = replace_executable(&exe_path, &dst) {
        eprintln!("could not install the update: {err}");
        eprintln!("the new build is at {}", dst.display());
        return 1;
    }
    println!("updated. Restart handoff to use {}", man.version);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_update_cases() {
        let cases = [
            ("newer available", "2026.8.18.0", "2026.8.19.0", false, true),
            ("same version", "2026.8.18.0", "2026.8.18.0", false, false),
            ("local build stays put", "dev", "2026.8.19.0", false, false),
            ("local build with force", "dev", "2026.8.19.0", true, true),
            (
                "manifest is older",
                "2026.9.1.0",
                "2026.8.19.0",
                false,
                false,
            ),
            (
                "unreadable manifest",
                "2026.8.18.0",
                "garbage",
                false,
                false,
            ),
        ];
        for (name, current, latest, force, want) in cases {
            assert_eq!(should_update(current, latest, force), want, "{name}");
        }
    }

    #[test]
    fn runtime_version_override_is_gone() {
        std::env::set_var("HANDOFF_VERSION", "9999.1.1.0");
        assert_eq!(get_current_version(), crate::VERSION);
        assert_ne!(get_current_version(), "9999.1.1.0");
    }

    #[test]
    fn replace_executable_moves_the_staged_build_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("handoff.exe");
        let staged = dir.path().join("handoff.exe.new");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        replace_executable(&exe, &staged).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!staged.exists());
    }

    #[test]
    fn replace_executable_restores_the_original_when_staging_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("handoff.exe");
        std::fs::write(&exe, b"old").unwrap();
        let err = replace_executable(&exe, &dir.path().join("absent")).unwrap_err();
        assert!(err.starts_with("install the new binary"), "{err}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    #[test]
    fn manifest_decodes_with_missing_fields() {
        let m: VersionManifest = serde_json::from_str(r#"{"version":"2026.8.19.0"}"#).unwrap();
        assert_eq!(m.version, "2026.8.19.0");
        assert!(m.sha256.is_empty());
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }
}

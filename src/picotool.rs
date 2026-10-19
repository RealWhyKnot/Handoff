use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::util;

#[cfg(feature = "embed_picotool")]
static DATA: &[u8] = include_bytes!("../assets/picotool/picotool.exe");

#[cfg(not(feature = "embed_picotool"))]
static DATA: &[u8] = &[];

pub fn embedded() -> bool {
    !DATA.is_empty()
}

pub fn path() -> Result<PathBuf, String> {
    if let Ok(p) = extract_once() {
        return Ok(p);
    }
    if let Some(p) = util::look_path("picotool") {
        return Ok(p);
    }
    if let Some(p) = util::look_path("picotool.exe") {
        return Ok(p);
    }
    Err("picotool not embedded and not on PATH (winget install raspberrypi.picotool)".to_string())
}

#[derive(Default)]
struct Extracted {
    path: Option<PathBuf>,
    error: Option<String>,
}

static EXTRACTED: Mutex<Extracted> = Mutex::new(Extracted {
    path: None,
    error: None,
});

fn extract_once() -> Result<PathBuf, String> {
    let mut state = EXTRACTED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = &state.path {
        return Ok(p.clone());
    }
    if let Some(e) = &state.error {
        return Err(e.clone());
    }
    match extract(DATA, &std::env::temp_dir().join("handoff")) {
        Ok(p) => {
            state.path = Some(p.clone());
            Ok(p)
        }
        Err(e) => {
            state.error = Some(e.clone());
            Err(e)
        }
    }
}

fn extract(data: &[u8], dir: &std::path::Path) -> Result<PathBuf, String> {
    if data.is_empty() {
        return Err("not embedded".to_string());
    }
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let sum = Sha256::digest(data);
    let name = format!("picotool-{}.exe", hex(&sum[..6]));
    let out = dir.join(name);

    if out.exists() {
        return Ok(out);
    }
    let tmp = PathBuf::from(format!("{}.part", out.display()));
    fs::write(&tmp, data).map_err(|e| e.to_string())?;
    if let Err(e) = fs::rename(&tmp, &out) {
        let _ = fs::remove_file(&tmp);
        if !out.exists() {
            return Err(e.to_string());
        }
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_names_the_file_after_the_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let first = extract(b"binary-one", dir.path()).unwrap();
        let again = extract(b"binary-one", dir.path()).unwrap();
        let other = extract(b"binary-two", dir.path()).unwrap();
        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(fs::read(&first).unwrap(), b"binary-one");
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("picotool-") && name.ends_with(".exe"),
            "{name}"
        );
        assert_eq!(name.len(), "picotool-".len() + 12 + ".exe".len());
    }

    #[test]
    fn empty_data_is_not_extractable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(extract(&[], dir.path()).is_err());
    }

    #[cfg(not(feature = "embed_picotool"))]
    #[test]
    fn default_builds_report_not_embedded() {
        assert!(!embedded());
    }
}

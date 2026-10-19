use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::util;

const MAX_ARG_VALUE_BYTES: usize = 512;

#[derive(Serialize, Default, Debug, Clone)]
pub struct Entry {
    pub ts: String,
    pub sid: String,
    pub op: String,
    pub cap: String,
    pub args: Option<Map<String, Value>>,
    pub consent: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub consent_scope: String,
    pub result: String,
    pub elapsed_ms: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

pub fn trim_args(args: &Map<String, Value>) -> Option<Map<String, Value>> {
    if args.is_empty() {
        return None;
    }
    let mut out = Map::new();
    for (key, value) in args {
        let encoded = serde_json::to_string(value).unwrap_or_default();
        if encoded.len() > MAX_ARG_VALUE_BYTES {
            out.insert(
                key.clone(),
                Value::String(format!("<elided {} bytes>", encoded.len())),
            );
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    Some(out)
}

pub fn default_dir() -> PathBuf {
    let root = util::env_var("PROGRAMDATA");
    let root = if root.is_empty() {
        std::env::temp_dir()
    } else {
        PathBuf::from(root)
    };
    root.join("whyknot").join("handoff").join("audit")
}

struct Inner {
    file: Option<File>,
    day: String,
}

pub struct Logger {
    dir: PathBuf,
    inner: Mutex<Inner>,
}

impl Logger {
    pub fn new() -> io::Result<Logger> {
        Logger::new_in_dir("")
    }

    pub fn new_in_dir(dir: &str) -> io::Result<Logger> {
        let dir = if dir.is_empty() {
            default_dir()
        } else {
            PathBuf::from(dir)
        };
        fs::create_dir_all(&dir)
            .map_err(|e| io::Error::new(e.kind(), format!("mkdir audit dir: {e}")))?;
        Ok(Logger {
            dir,
            inner: Mutex::new(Inner {
                file: None,
                day: String::new(),
            }),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn write(&self, mut entry: Entry) -> io::Result<()> {
        if entry.ts.is_empty() {
            entry.ts = util::utc_timestamp();
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("audit log lock poisoned"))?;

        let day = util::utc_day();
        if inner.file.is_none() || inner.day != day {
            inner.file = None;
            let path = self.dir.join(format!("handoff-{day}.jsonl"));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| io::Error::new(e.kind(), format!("open audit file: {e}")))?;
            inner.file = Some(file);
            inner.day = day;
        }
        let mut line = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        line.push(b'\n');
        match inner.file.as_mut() {
            Some(file) => file.write_all(&line),
            None => Ok(()),
        }
    }

    pub fn close(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.file = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn trim_args_returns_none_for_empty_args() {
        assert!(trim_args(&Map::new()).is_none());
    }

    #[test]
    fn trim_args_elides_bulky_values() {
        let big = "a".repeat(600);
        let trimmed = trim_args(&args(json!({"path": "C:\\x", "data_base64": big}))).unwrap();
        assert_eq!(trimmed["path"], json!("C:\\x"));
        let note = trimmed["data_base64"].as_str().unwrap();
        assert!(note.starts_with("<elided "), "{note}");
    }

    #[test]
    fn entries_serialize_in_the_documented_order() {
        let entry = Entry {
            ts: "2026-01-02T03:04:05.006Z".into(),
            sid: "n1_abc".into(),
            cap: "sys.info".into(),
            consent: "not_required".into(),
            result: "ok".into(),
            elapsed_ms: 12,
            ..Entry::default()
        };
        assert_eq!(
            serde_json::to_string(&entry).unwrap(),
            r#"{"ts":"2026-01-02T03:04:05.006Z","sid":"n1_abc","op":"","cap":"sys.info","args":null,"consent":"not_required","result":"ok","elapsed_ms":12}"#
        );
    }

    #[test]
    fn optional_fields_appear_when_set() {
        let entry = Entry {
            consent_scope: "files".into(),
            detail: "the host declined this request".into(),
            ..Entry::default()
        };
        let text = serde_json::to_string(&entry).unwrap();
        assert!(text.contains(r#""consent_scope":"files""#));
        assert!(text.contains(r#""detail":"the host declined this request""#));
    }

    #[test]
    fn writes_one_line_per_entry_to_a_dated_file() {
        let dir = tempfile::tempdir().unwrap();
        let logger = Logger::new_in_dir(dir.path().to_str().unwrap()).unwrap();
        for _ in 0..2 {
            logger
                .write(Entry {
                    cap: "sys.info".into(),
                    result: "ok".into(),
                    ..Entry::default()
                })
                .unwrap();
        }
        logger.close();
        let name = format!("handoff-{}.jsonl", util::utc_day());
        let text = fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(text.lines().count(), 2);
        let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert!(first["ts"].as_str().unwrap().ends_with('Z'));
    }
}

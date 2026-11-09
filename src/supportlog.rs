use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::util;

pub const MAX_BYTES: u64 = 2 << 20;

struct Logger {
    file: Option<File>,
    path: PathBuf,
    lines: u64,
}

static CURRENT: Mutex<Option<Logger>> = Mutex::new(None);
static DIR_OVERRIDE: Mutex<String> = Mutex::new(String::new());

pub fn set_dir(dir: &str) {
    if let Ok(mut guard) = DIR_OVERRIDE.lock() {
        *guard = dir.trim().to_string();
    }
}

pub fn init() -> io::Result<PathBuf> {
    let path = path().map_err(io::Error::other)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    rotate_if_large(&path);
    let file = open_append(&path)?;
    if let Ok(mut guard) = CURRENT.lock() {
        *guard = Some(Logger {
            file: Some(file),
            path: path.clone(),
            lines: 0,
        });
    }
    write(format_args!(
        "handoff start pid={} exe={:?}",
        std::process::id(),
        current_exe_string()
    ));
    Ok(path)
}

pub fn path() -> Result<PathBuf, String> {
    let override_dir = DIR_OVERRIDE.lock().map(|g| g.clone()).unwrap_or_default();
    if !override_dir.is_empty() {
        return Ok(Path::new(&override_dir).join("handoff.log"));
    }
    if let Some(base) = util::user_cache_dir() {
        return Ok(base
            .join("whyknot")
            .join("handoff")
            .join("logs")
            .join("handoff.log"));
    }
    match std::env::current_exe() {
        Ok(exe) => Ok(path_for_executable(&exe)),
        Err(_) => Err("resolve log directory: user cache directory is not defined".to_string()),
    }
}

pub fn path_for_executable(exe: &Path) -> PathBuf {
    let dir = exe.parent().unwrap_or_else(|| Path::new(""));
    let base = exe
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match base.rfind('.') {
        Some(i) => &base[..i],
        None => &base[..],
    };
    let name = if name.is_empty() { "handoff" } else { name };
    dir.join(format!("{name}.log"))
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotate_if_large(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() >= MAX_BYTES {
        rotate(path);
    }
}

fn rotate(path: &Path) {
    let old = PathBuf::from(format!("{}.1", path.display()));
    let _ = fs::remove_file(&old);
    let _ = fs::rename(path, &old);
}

pub fn write(args: fmt::Arguments<'_>) {
    let Ok(mut guard) = CURRENT.lock() else {
        return;
    };
    let Some(logger) = guard.as_mut() else {
        return;
    };
    let Some(file) = logger.file.as_mut() else {
        return;
    };
    let _ = writeln!(file, "{} {}", util::utc_timestamp(), args);

    logger.lines += 1;
    if logger.lines % 256 != 0 {
        return;
    }
    let too_big = file
        .metadata()
        .map(|m| m.len() >= MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        logger.file = None;
        rotate(&logger.path);
        logger.file = open_append(&logger.path).ok();
    }
}

pub struct FileLayer;

struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        }
    }
}

impl<S: Subscriber> Layer<S> for FileLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !event.metadata().target().starts_with("handoff") {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        write(format_args!("{}", message.0));
    }
}

pub fn close() {
    if let Ok(mut guard) = CURRENT.lock() {
        *guard = None;
    }
}

fn current_exe_string() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_for_executable_writes_beside_exe() {
        let exe = Path::new("tools").join("handoff.exe");
        assert_eq!(
            path_for_executable(&exe),
            Path::new("tools").join("handoff.log")
        );
    }

    #[test]
    fn path_for_executable_uses_executable_base_name() {
        let exe = Path::new("temp").join("handoff-preview.exe");
        assert_eq!(
            path_for_executable(&exe),
            Path::new("temp").join("handoff-preview.log")
        );
    }

    #[test]
    fn path_for_executable_falls_back_for_extension_only_names() {
        let exe = Path::new("temp").join(".exe");
        assert_eq!(
            path_for_executable(&exe),
            Path::new("temp").join("handoff.log")
        );
    }

    #[test]
    fn rotation_moves_the_log_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handoff.log");
        fs::write(&path, vec![b'x'; usize::try_from(MAX_BYTES + 1).unwrap()]).unwrap();
        rotate_if_large(&path);
        assert!(!path.exists());
        assert!(dir.path().join("handoff.log.1").exists());
    }

    #[test]
    fn small_logs_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handoff.log");
        fs::write(&path, b"small").unwrap();
        rotate_if_large(&path);
        assert!(path.exists());
    }

    #[test]
    fn events_from_this_crate_reach_the_log_file() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let dir = tempfile::tempdir().unwrap();
        set_dir(&dir.path().display().to_string());
        let path = init().unwrap();
        let subscriber = tracing_subscriber::registry().with(FileLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("session start relay={} version={}", "r", 7);
            tracing::info!(target: "hyper", "not ours");
        });
        close();
        set_dir("");
        let text = fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].contains(" handoff start pid="), "{text}");
        assert!(
            lines[1].ends_with("Z session start relay=r version=7"),
            "{text}"
        );
    }
}

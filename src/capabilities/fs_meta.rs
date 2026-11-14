use std::fs;
use std::io::{self};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn fmt_time(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        Err(before) => {
            let gap = before.duration();
            -(i64::try_from(gap.as_secs()).unwrap_or(i64::MAX)) - i64::from(gap.subsec_nanos() > 0)
        }
    };
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    if month <= 2 {
        year += 1;
    }
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

const ATTR_READONLY: u32 = 0x1;

const ATTR_DIRECTORY: u32 = 0x10;

const ATTR_REPARSE: u32 = 0x400;

const TAG_SYMLINK: u32 = 0xA000_000C;

const TAG_MOUNT_POINT: u32 = 0xA000_0003;

const TAG_DEDUP: u32 = 0x8000_0013;

#[cfg(windows)]
fn attrs_of(md: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}

#[cfg(not(windows))]
fn attrs_of(md: &fs::Metadata) -> u32 {
    if md.is_dir() { ATTR_DIRECTORY } else { 0 }
}

#[cfg(windows)]
fn reparse_tag(path: &Path) -> u32 {
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    struct FindData {
        attrs: u32,
        times: [u32; 6],
        size_high: u32,
        size_low: u32,
        reserved0: u32,
        reserved1: u32,
        file_name: [u16; 260],
        alt_name: [u16; 14],
    }

    unsafe extern "system" {
        fn FindFirstFileW(name: *const u16, data: *mut FindData) -> isize;
        fn FindClose(handle: isize) -> i32;
    }

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut data = FindData {
        attrs: 0,
        times: [0; 6],
        size_high: 0,
        size_low: 0,
        reserved0: 0,
        reserved1: 0,
        file_name: [0; 260],
        alt_name: [0; 14],
    };
    // SAFETY: wide is NUL-terminated and data is a live FindData the call fills in.
    let handle = unsafe { FindFirstFileW(wide.as_ptr(), &raw mut data) };
    if handle == -1 {
        return 0;
    }
    // SAFETY: handle came from FindFirstFileW above and is closed only here.
    unsafe { FindClose(handle) };
    data.reserved0
}

#[cfg(not(windows))]
fn reparse_tag(_path: &Path) -> u32 {
    0
}

pub(crate) struct Info {
    pub size: u64,
    pub mtime: SystemTime,
    pub dir: bool,
    pub symlink: bool,
    pub mode: String,
}

pub(crate) fn info_of(md: &fs::Metadata, path: &Path) -> Info {
    let attrs = attrs_of(md);
    let mut perm: u32 = if attrs & ATTR_READONLY != 0 {
        0o444
    } else {
        0o666
    };
    let tag = if attrs & ATTR_REPARSE != 0 {
        reparse_tag(path)
    } else {
        0
    };
    let mut dir = false;
    let mut irregular = false;
    let symlink = tag == TAG_SYMLINK || tag == TAG_MOUNT_POINT;
    if !symlink {
        if attrs & ATTR_DIRECTORY != 0 {
            dir = true;
            perm |= 0o111;
        }
        if tag != 0 && tag != TAG_DEDUP {
            irregular = true;
        }
    }
    let mut mode = String::new();
    if dir {
        mode.push('d');
    }
    if symlink {
        mode.push('L');
    }
    if irregular {
        mode.push('?');
    }
    if mode.is_empty() {
        mode.push('-');
    }
    for (i, c) in "rwxrwxrwx".chars().enumerate() {
        mode.push(if perm & (1 << (8 - i)) != 0 { c } else { '-' });
    }
    Info {
        size: md.len(),
        mtime: md.modified().unwrap_or(UNIX_EPOCH),
        dir,
        symlink,
        mode,
    }
}

pub(crate) fn stat(path: &str) -> io::Result<Info> {
    let md = fs::metadata(path)?;
    Ok(info_of(&md, Path::new(path)))
}

pub(crate) fn lstat(path: &str) -> io::Result<Info> {
    let md = fs::symlink_metadata(path)?;
    Ok(info_of(&md, Path::new(path)))
}

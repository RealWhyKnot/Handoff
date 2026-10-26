use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::util;

static COLOR: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Dim,
    Bold,
    Green,
    Red,
    Yellow,
    Cyan,
}

pub fn init() -> bool {
    let on =
        color_wanted(&util::env_var("NO_COLOR"), std::io::stdout().is_terminal()) && enable_vt();
    COLOR.store(on, Ordering::SeqCst);
    on
}

pub fn color_wanted(no_color: &str, stdout_is_terminal: bool) -> bool {
    no_color.is_empty() && stdout_is_terminal
}

pub fn color_on() -> bool {
    COLOR.load(Ordering::SeqCst)
}

#[cfg(test)]
pub fn set_color(on: bool) {
    COLOR.store(on, Ordering::SeqCst);
}

#[cfg(windows)]
fn enable_vt() -> bool {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let Ok(handle) = GetStdHandle(STD_OUTPUT_HANDLE) else {
            return false;
        };
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(handle, &mut mode).is_err() {
            return false;
        }
        if mode.0 & ENABLE_VIRTUAL_TERMINAL_PROCESSING.0 != 0 {
            return true;
        }
        SetConsoleMode(
            handle,
            CONSOLE_MODE(mode.0 | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0),
        )
        .is_ok()
    }
}

#[cfg(not(windows))]
fn enable_vt() -> bool {
    true
}

pub fn paint(tone: Tone, text: &str) -> String {
    if !color_on() || text.is_empty() {
        return text.to_string();
    }
    let code = match tone {
        Tone::Dim => "90",
        Tone::Bold => "1",
        Tone::Green => "32",
        Tone::Red => "31",
        Tone::Yellow => "33",
        Tone::Cyan => "36",
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

pub fn clock() -> String {
    let (h, m, s) = local_hms();
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(windows)]
fn local_hms() -> (u16, u16, u16) {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        millis: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(time: *mut SystemTime);
    }
    let mut t = SystemTime::default();
    unsafe { GetLocalTime(&mut t) };
    (t.hour, t.minute, t.second)
}

#[cfg(not(windows))]
fn local_hms() -> (u16, u16, u16) {
    let secs = util::now_ms().div_euclid(1000).rem_euclid(86_400);
    (
        (secs / 3600) as u16,
        ((secs % 3600) / 60) as u16,
        (secs % 60) as u16,
    )
}

pub fn stamp(text: &str) -> String {
    format!("{}  {}", paint(Tone::Dim, &clock()), text)
}

pub fn line(text: &str) {
    println!("{}", stamp(text));
}

pub fn eline(text: &str) {
    eprintln!("{}", stamp(text));
}

pub fn human_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 100 {
        return "under 0.1 s".to_string();
    }
    if ms < 9_950 {
        let tenths = (ms + 50) / 100;
        return format!("{}.{} s", tenths / 10, tenths % 10);
    }
    let secs = (ms + 500) / 1000;
    if secs < 60 {
        return format!("{secs} s");
    }
    if secs < 3600 {
        let (m, s) = (secs / 60, secs % 60);
        return if s == 0 {
            format!("{m} min")
        } else {
            format!("{m} min {s} s")
        };
    }
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if m == 0 {
        format!("{h} h")
    } else {
        format!("{h} h {m} min")
    }
}

pub fn human_ms(ms: i64) -> String {
    human_duration(Duration::from_millis(ms.max(0) as u64))
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return if n == 1 {
            "1 byte".to_string()
        } else {
            format!("{n} bytes")
        };
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{:.1} {}", value, UNITS[unit])
    } else {
        format!("{:.0} {}", value, UNITS[unit])
    }
}

pub fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

const PLAIN_ERRORS: [(&str, &str); 5] = [
    (
        "context deadline exceeded (Client.Timeout exceeded while awaiting headers)",
        "it took too long to answer",
    ),
    ("context deadline exceeded", "it took too long"),
    ("context canceled", "it was stopped"),
    ("context cancelled", "it was stopped"),
    ("signal: killed", "the program was stopped"),
];

pub fn plain_error(text: &str) -> String {
    let mut out = text.replace(['\r', '\n'], " ");
    for (from, to) in PLAIN_ERRORS {
        out = out.replace(from, to);
    }
    replace_go_durations(&out)
}

fn replace_go_durations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, word) in text.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let core = word.trim_end_matches([',', '.', ';', ':', ')']);
        let tail = &word[core.len()..];
        match go_duration_word(core) {
            Some(d) => {
                out.push_str(&human_duration(d));
                out.push_str(tail);
            }
            None => out.push_str(word),
        }
    }
    out
}

fn go_duration_word(word: &str) -> Option<Duration> {
    if word.is_empty() || !word.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    if !(word.ends_with('s') || word.ends_with('m') || word.ends_with('h')) {
        return None;
    }
    if !word.contains(['h', 'm']) && !word.ends_with("ms") {
        return None;
    }
    util::parse_go_duration(word)
}

pub fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    match text.find(". ") {
        Some(i) => &text[..=i],
        None => text,
    }
}

pub fn display_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        width += char_width(c);
    }
    width
}

pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if c.is_control() || zero_width(cp) {
        return 0;
    }
    if wide(cp) {
        2
    } else {
        1
    }
}

fn zero_width(cp: u32) -> bool {
    matches!(
        cp,
        0x0300..=0x036F
            | 0x0483..=0x0489
            | 0x0591..=0x05BD
            | 0x0610..=0x061A
            | 0x064B..=0x065F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
            | 0xFEFF
            | 0xE0000..=0xE0FFF
    )
}

fn wide(cp: u32) -> bool {
    matches!(
        cp,
        0x1100..=0x115F
            | 0x231A..=0x231B
            | 0x2329..=0x232A
            | 0x23E9..=0x23F3
            | 0x25FD..=0x25FE
            | 0x2600..=0x27BF
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F000..=0x1FAFF
            | 0x20000..=0x3FFFD
    )
}

pub const STAMP_WIDTH: usize = 10;
const DEFAULT_COLUMNS: usize = 100;
const MIN_COLUMNS: usize = 40;
const MAX_COLUMNS: usize = 400;

pub fn columns() -> usize {
    terminal_columns()
        .unwrap_or(DEFAULT_COLUMNS)
        .clamp(MIN_COLUMNS, MAX_COLUMNS)
}

pub fn body_width() -> usize {
    body_width_for(columns())
}

pub fn body_width_for(columns: usize) -> usize {
    columns.max(MIN_COLUMNS) - 1 - STAMP_WIDTH
}

#[cfg(windows)]
fn terminal_columns() -> Option<usize> {
    use windows::Win32::System::Console::{
        GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_OUTPUT_HANDLE,
    };
    if !std::io::stdout().is_terminal() {
        return None;
    }
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE).ok()?;
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        GetConsoleScreenBufferInfo(handle, &mut info).ok()?;
        let cols = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
        usize::try_from(cols).ok().filter(|c| *c > 0)
    }
}

#[cfg(not(windows))]
fn terminal_columns() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    util::env_var("COLUMNS").trim().parse().ok()
}

pub fn stamp_lines(lines: &[String]) -> String {
    let indent = " ".repeat(STAMP_WIDTH);
    let mut out = String::new();
    for (i, text) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(&stamp(text));
        } else {
            out.push('\n');
            out.push_str(&indent);
            out.push_str(text);
        }
    }
    out
}

pub fn lines(lines: &[String]) {
    println!("{}", stamp_lines(lines));
}

pub fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - w))
}

pub fn table(headers: &[String], rows: &[Vec<String>], max_cell: usize) -> String {
    let cut = |s: &str| -> String {
        let s = s.replace(['\r', '\n', '\t'], " ");
        if s.chars().count() <= max_cell {
            return s;
        }
        let mut t: String = s.chars().take(max_cell.saturating_sub(3)).collect();
        t.push_str("...");
        t
    };
    let headers: Vec<String> = headers.iter().map(|h| cut(h)).collect();
    let rows: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| cut(c)).collect())
        .collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let render = |cells: &[String]| -> String {
        let last = cells.len().saturating_sub(1);
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i == last {
                line.push_str(cell);
            } else {
                line.push_str(&pad(cell, widths[i]));
                line.push_str("  ");
            }
        }
        line.trim_end().to_string()
    };
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(paint(Tone::Bold, &render(&headers)));
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    out.push(paint(Tone::Dim, &render(&rule)));
    for row in &rows {
        out.push(render(row));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_follows_no_color_and_the_terminal() {
        assert!(color_wanted("", true));
        assert!(!color_wanted("1", true));
        assert!(!color_wanted("", false));
    }

    #[test]
    fn durations_read_like_a_person_would_say_them() {
        let ms = |n| human_duration(Duration::from_millis(n));
        assert_eq!(ms(40), "under 0.1 s");
        assert_eq!(ms(1360), "1.4 s");
        assert_eq!(ms(9_940), "9.9 s");
        assert_eq!(ms(9_960), "10 s");
        assert_eq!(ms(42_000), "42 s");
        assert_eq!(ms(125_000), "2 min 5 s");
        assert_eq!(ms(900_000), "15 min");
        assert_eq!(ms(3_600_000), "1 h");
        assert_eq!(ms(3_900_000), "1 h 5 min");
        assert_eq!(human_ms(-5), "under 0.1 s");
    }

    #[test]
    fn bytes_are_rounded_to_a_readable_unit() {
        assert_eq!(human_bytes(1), "1 byte");
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(50 * 1024 * 1024), "50 MB");
    }

    #[test]
    fn go_wording_is_replaced() {
        assert_eq!(
            plain_error("Post \"x\": context deadline exceeded"),
            "Post \"x\": it took too long"
        );
        assert_eq!(plain_error("signal: killed"), "the program was stopped");
        assert_eq!(
            plain_error("still running after 15m0s, try later"),
            "still running after 15 min, try later"
        );
        assert_eq!(plain_error("waited 1m30s."), "waited 1 min 30 s.");
        assert_eq!(plain_error("port 80s"), "port 80s");
        assert_eq!(plain_error("line\none"), "line one");
    }

    #[test]
    fn first_sentence_stops_at_the_first_full_stop() {
        assert_eq!(
            first_sentence("Deletes a file. It cannot be undone."),
            "Deletes a file."
        );
        assert_eq!(first_sentence("No full stop"), "No full stop");
    }

    #[test]
    fn tables_line_up_and_cut_long_cells() {
        set_color(false);
        let t = table(
            &["Name".into(), "PID".into()],
            &[
                vec!["explorer.exe".into(), "4120".into()],
                vec!["a".repeat(50), "7".into()],
            ],
            20,
        );
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "Name                  PID");
        assert_eq!(lines[1], "--------------------  ----");
        assert_eq!(lines[2], "explorer.exe          4120");
        assert_eq!(lines[3], format!("{}...  7", "a".repeat(17)));
    }

    #[test]
    fn wide_and_zero_width_characters_are_measured() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("网络"), 4);
        assert_eq!(display_width("\u{1F680}"), 2);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(display_width("a\u{200B}b\u{FEFF}"), 2);
        assert_eq!(display_width("\u{1b}[1m网\u{1b}[0mx"), 3);
        assert_eq!(char_width('\u{7}'), 0);
    }

    #[test]
    fn body_width_leaves_room_for_the_clock_and_the_last_column() {
        assert_eq!(body_width_for(80), 69);
        assert_eq!(body_width_for(120), 109);
        assert_eq!(body_width_for(10), 29);
    }

    #[test]
    fn continuation_lines_line_up_under_the_text() {
        set_color(false);
        let text = stamp_lines(&["#1  a".to_string(), "  b".to_string()]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), STAMP_WIDTH + 5);
        assert_eq!(lines[1], format!("{}  b", " ".repeat(STAMP_WIDTH)));
    }

    #[test]
    fn width_ignores_colour_codes() {
        assert_eq!(display_width("\x1b[32mok\x1b[0m"), 2);
        assert_eq!(pad("\x1b[1mab\x1b[0m", 4), "\x1b[1mab\x1b[0m  ");
    }
}

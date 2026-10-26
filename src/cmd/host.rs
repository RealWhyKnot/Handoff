use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::layout;
use super::summary::{ArgView, BODY_KEYS};
use crate::consent::{self, ConsentError, Event, Step};
use crate::console::{self, display_width, human_duration, plural, Tone};
use crate::dispatch::{Outcome, Spec};

pub const PRESENCE_SETTLE: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone)]
pub struct Running {
    pub num: usize,
    pub id: String,
    pub kind: String,
    pub started: Instant,
    consent: Asked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Not,
    Waiting,
    Answered,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub run: usize,
    pub done: usize,
    pub refused: usize,
    pub failed: usize,
    pub stopped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Done,
    Refused,
    Failed,
    Stopped,
}

#[derive(Default)]
struct Viewers {
    shown: usize,
    latest: usize,
}

pub struct Board {
    started: Instant,
    link: String,
    read_only: bool,
    labels: Mutex<HashMap<String, String>>,
    next: AtomicUsize,
    running: Mutex<Vec<Running>>,
    stats: Mutex<Stats>,
    viewers: Mutex<Viewers>,
    presence_gen: AtomicU64,
}

fn guard<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Board {
    pub fn new(link: &str, read_only: bool) -> Board {
        Board {
            started: Instant::now(),
            link: link.to_string(),
            read_only,
            labels: Mutex::new(HashMap::new()),
            next: AtomicUsize::new(1),
            running: Mutex::new(Vec::new()),
            stats: Mutex::new(Stats::default()),
            viewers: Mutex::new(Viewers::default()),
            presence_gen: AtomicU64::new(0),
        }
    }

    pub fn link(&self) -> &str {
        &self.link
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn learn_labels(&self, specs: &[Spec]) {
        let mut labels = guard(&self.labels);
        for spec in specs {
            if !spec.label.is_empty() {
                labels.insert(spec.kind.clone(), spec.label.clone());
            }
        }
    }

    pub fn label(&self, kind: &str) -> Option<String> {
        guard(&self.labels).get(kind).cloned()
    }

    pub fn begin(&self, id: &str, kind: &str) -> usize {
        let num = self.next.fetch_add(1, Ordering::SeqCst);
        guard(&self.running).push(Running {
            num,
            id: id.to_string(),
            kind: kind.to_string(),
            started: Instant::now(),
            consent: Asked::Not,
        });
        guard(&self.stats).run += 1;
        num
    }

    pub fn finish(&self, id: &str, ending: Ending) -> Option<Running> {
        let mut running = guard(&self.running);
        let at = running.iter().position(|r| r.id == id)?;
        let job = running.remove(at);
        drop(running);
        let mut stats = guard(&self.stats);
        match ending {
            Ending::Done => stats.done += 1,
            Ending::Refused => stats.refused += 1,
            Ending::Failed => stats.failed += 1,
            Ending::Stopped => stats.stopped += 1,
        }
        Some(job)
    }

    pub fn number_of(&self, id: &str) -> Option<usize> {
        guard(&self.running)
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.num)
    }

    pub fn running(&self) -> Vec<Running> {
        guard(&self.running).clone()
    }

    pub fn stats(&self) -> Stats {
        *guard(&self.stats)
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn consent_target(&self, kind: &str, step: &Step) -> Option<usize> {
        let mut running = guard(&self.running);
        let (from, to) = match step {
            Step::Asking => (Asked::Not, Asked::Waiting),
            _ => (Asked::Waiting, Asked::Answered),
        };
        let job = running
            .iter_mut()
            .find(|r| r.kind == kind && r.consent == from)?;
        job.consent = to;
        Some(job.num)
    }

    pub fn on_consent(&self, event: &Event) {
        let num = self.consent_target(&event.request.kind, &event.step);
        let text = consent_line(event);
        match num {
            Some(n) => console::line(&format!("{}  {text}", tag(n))),
            None => console::line(&text),
        }
    }

    pub fn see_viewers(&self, count: usize) -> u64 {
        guard(&self.viewers).latest = count;
        self.presence_gen.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn settle_viewers(&self, generation: u64) -> Option<String> {
        if self.presence_gen.load(Ordering::SeqCst) != generation {
            return None;
        }
        let mut v = guard(&self.viewers);
        let text = presence_line(v.shown, v.latest)?;
        v.shown = v.latest;
        Some(text)
    }

    pub fn watch_consent(self: &Arc<Self>) {
        let board = self.clone();
        consent::set_observer(Some(Arc::new(move |event: &Event| board.on_consent(event))));
    }
}

pub fn tag(num: usize) -> String {
    console::paint(Tone::Bold, &format!("#{num}"))
}

pub fn presence_line(before: usize, now: usize) -> Option<String> {
    if before == now {
        return None;
    }
    Some(match (before, now) {
        (0, 1) => "Someone opened your link.".to_string(),
        (0, n) => format!("Your link was opened in {n} places."),
        (_, 0) => {
            "The helper closed the page. The same link still works if they come back.".to_string()
        }
        (b, n) if n > b => format!("Someone else opened your link. It's open in {n} places now."),
        (_, 1) => "One page was closed. Your link is still open in 1 place.".to_string(),
        (_, n) => format!("One page was closed. Your link is still open in {n} places."),
    })
}

pub fn consent_line(event: &Event) -> String {
    match &event.step {
        Step::Asking => console::paint(
            Tone::Yellow,
            "This would change your computer, so Handoff is asking you first. Look for the question on your screen.",
        ),
        Step::Allowed(term) => format!("You allowed it {}.", term.text()),
        Step::Denied => "You said no.".to_string(),
        Step::TimedOut => format!(
            "Nobody answered within {}, so Handoff said no for you.",
            human_duration(consent::PROMPT_TIMEOUT)
        ),
        Step::Withdrawn => {
            "The helper cancelled the request, so the question was closed.".to_string()
        }
        Step::Failed(why) => format!(
            "Handoff couldn't show the question ({}), so it said no.",
            console::plain_error(why)
        ),
    }
}

pub fn ending_for(out: &Outcome, stopped: bool) -> Ending {
    if out.ok {
        return Ending::Done;
    }
    if stopped {
        return Ending::Stopped;
    }
    if is_refusal(&out.error) {
        return Ending::Refused;
    }
    Ending::Failed
}

pub fn is_refusal(error: &str) -> bool {
    [
        ConsentError::Denied.to_string(),
        ConsentError::TimedOut.to_string(),
    ]
    .iter()
    .any(|t| error == t)
}

fn result_parts(out: &Outcome, ending: Ending, read_only: bool) -> (String, Tone, Option<Tone>) {
    let took = human_duration(Duration::from_millis(out.elapsed_ms.max(0) as u64));
    match ending {
        Ending::Done => (format!("Done in {took}."), Tone::Green, None),
        Ending::Refused => {
            let why = if read_only {
                "Not allowed. This session can only look, not make changes."
            } else if out.error == ConsentError::TimedOut.to_string() {
                "Not allowed, because nobody answered."
            } else {
                "Not allowed, because you said no."
            };
            (why.to_string(), Tone::Yellow, None)
        }
        Ending::Stopped => (
            format!("Stopped after {took}."),
            Tone::Yellow,
            Some(Tone::Yellow),
        ),
        Ending::Failed => {
            let head = if out.elapsed_ms < 100 {
                "Failed:".to_string()
            } else {
                format!("Failed after {took}:")
            };
            (head, Tone::Red, None)
        }
    }
}

fn shown_detail(ending: Ending, detail: &str) -> String {
    match ending {
        Ending::Refused => String::new(),
        _ => layout::clean_inline(detail),
    }
}

pub fn result_line(out: &Outcome, ending: Ending, read_only: bool, detail: &str) -> String {
    let (head, tone, detail_tone) = result_parts(out, ending, read_only);
    let detail = shown_detail(ending, detail);
    let head = console::paint(tone, &head);
    if detail.is_empty() {
        return head;
    }
    format!("{head} {}", paint_maybe(detail_tone, &detail))
}

pub fn result_lines(
    num: usize,
    out: &Outcome,
    ending: Ending,
    read_only: bool,
    detail: &str,
    width: usize,
) -> Vec<String> {
    let (head, tone, detail_tone) = result_parts(out, ending, read_only);
    let detail = shown_detail(ending, detail);
    let mut pieces: Vec<Piece> = head
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(|w| Piece::new(w, Some(tone)))
        .collect();
    pieces.extend(
        detail
            .split(' ')
            .filter(|w| !w.is_empty())
            .map(|w| Piece::new(w, detail_tone)),
    );
    flow(&lead(num), &pieces, " ", width, RESULT_ROWS)
}

pub fn exit_summary(stats: Stats, length: Duration) -> String {
    let took = human_duration(length);
    if stats.run == 0 {
        return format!("No commands ran. The session lasted {took}.");
    }
    let mut parts = Vec::new();
    if stats.done > 0 {
        parts.push(format!("{} worked", stats.done));
    }
    if stats.refused > 0 {
        parts.push(format!("{} not allowed", stats.refused));
    }
    if stats.failed > 0 {
        parts.push(format!("{} failed", stats.failed));
    }
    if stats.stopped > 0 {
        parts.push(format!("{} stopped", stats.stopped));
    }
    let finished = stats.done + stats.refused + stats.failed + stats.stopped;
    if finished < stats.run {
        parts.push(format!("{} didn't finish", stats.run - finished));
    }
    format!(
        "The helper ran {} in {took}: {}.",
        plural(stats.run, "command", "commands"),
        join_words(&parts)
    )
}

fn join_words(parts: &[String]) -> String {
    match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        2 => format!("{} and {}", parts[0], parts[1]),
        n => format!("{}, and {}", parts[..n - 1].join(", "), parts[n - 1]),
    }
}

const HEADER_ROWS: usize = 4;
const SCRIPT_ROWS: usize = 12;
const VALUE_ROWS: usize = 6;
const RESULT_ROWS: usize = 6;
const BODY_INDENT: &str = "  ";
const GUTTER: &str = "\u{2502}";
const MAX_KEY: usize = 24;

struct Piece {
    text: String,
    tone: Option<Tone>,
}

impl Piece {
    fn new(text: &str, tone: Option<Tone>) -> Piece {
        Piece {
            text: text.to_string(),
            tone,
        }
    }
}

fn paint_maybe(tone: Option<Tone>, text: &str) -> String {
    match tone {
        Some(tone) => console::paint(tone, text),
        None => text.to_string(),
    }
}

fn lead(num: usize) -> Piece {
    Piece::new(&format!("#{num}"), Some(Tone::Bold))
}

fn flow(lead: &Piece, pieces: &[Piece], sep: &str, width: usize, max_rows: usize) -> Vec<String> {
    let lead_w = display_width(&lead.text);
    let indent = (lead_w + 2).min(width / 2);
    let room = width - indent;
    let sep_w = display_width(sep);
    let mut rows: Vec<String> = vec![paint_maybe(lead.tone, &lead.text)];
    let mut used = lead_w;
    for (i, piece) in pieces.iter().enumerate() {
        let joint = if i == 0 { "  " } else { sep };
        let gap = if i == 0 { 2 } else { sep_w };
        let w = display_width(&piece.text);
        if used + gap + w <= width {
            if let Some(row) = rows.last_mut() {
                row.push_str(joint);
                row.push_str(&paint_maybe(piece.tone, &piece.text));
            }
            used += gap + w;
            continue;
        }
        for chunk in layout::wrap(&piece.text, room, room) {
            used = indent + display_width(&chunk);
            rows.push(format!(
                "{}{}",
                " ".repeat(indent),
                paint_maybe(piece.tone, &chunk)
            ));
        }
    }
    if rows.len() > max_rows.max(1) {
        rows.truncate(max_rows.max(1));
        if let Some(last) = rows.last_mut() {
            if display_width(last) + 4 <= width {
                last.push_str(" ...");
            } else {
                *last = format!("{}...", " ".repeat(indent));
            }
        }
    }
    rows
}

pub fn command_lines(
    num: usize,
    label: Option<&str>,
    kind: &str,
    args: &ArgView,
    width: usize,
) -> Vec<String> {
    let mut pieces = Vec::new();
    match label {
        Some(label) => {
            pieces.push(Piece::new(&layout::clean_inline(label), Some(Tone::Cyan)));
            pieces.push(Piece::new(&layout::clean_inline(kind), Some(Tone::Dim)));
        }
        None => pieces.push(Piece::new(&layout::clean_inline(kind), Some(Tone::Cyan))),
    }
    pieces.extend(args.inline.iter().map(|a| Piece::new(a, None)));
    let mut rows = flow(&lead(num), &pieces, "  ", width, HEADER_ROWS);
    for (key, text) in &args.blocks {
        rows.extend(block_rows(key, text, width));
    }
    rows
}

fn block_rows(key: &str, text: &str, width: usize) -> Vec<String> {
    let lines = layout::clean_lines(text);
    if lines.is_empty() {
        return Vec::new();
    }
    let key = short_key(key, width);
    let code = BODY_KEYS.contains(&key.as_str());
    let mut rows = Vec::new();
    if code || lines.len() > 1 {
        if !code {
            rows.push(format!("{BODY_INDENT}{}", console::paint(Tone::Dim, &key)));
        }
        let gutter = format!("{BODY_INDENT}{} ", console::paint(Tone::Dim, GUTTER));
        let inner = width - display_width(&gutter);
        let folded = layout::fold(&lines, inner, SCRIPT_ROWS, 2);
        for row in folded.rows {
            rows.push(format!("{gutter}{row}").trim_end().to_string());
        }
        if let Some(more) = folded.more {
            for piece in layout::wrap(&more.note(), inner, inner) {
                rows.push(format!("{gutter}{}", console::paint(Tone::Dim, &piece)));
            }
        }
        return rows;
    }
    let pad = " ".repeat(display_width(BODY_INDENT) + display_width(&key) + 2);
    let inner = width - pad.len();
    let folded = layout::fold(&lines, inner, VALUE_ROWS, 0);
    for (i, row) in folded.rows.iter().enumerate() {
        if i == 0 {
            rows.push(format!(
                "{BODY_INDENT}{}  {row}",
                console::paint(Tone::Dim, &key)
            ));
        } else {
            rows.push(format!("{pad}{row}"));
        }
    }
    if let Some(more) = folded.more {
        for piece in layout::wrap(&more.note(), inner, inner) {
            rows.push(format!("{pad}{}", console::paint(Tone::Dim, &piece)));
        }
    }
    rows
}

fn short_key(key: &str, width: usize) -> String {
    let max = MAX_KEY.min(width / 4).max(1);
    let mut out = String::new();
    let mut used = 0;
    for c in layout::clean_inline(key).chars() {
        let w = console::char_width(c);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

pub fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(windows)]
    {
        use std::io::Write;
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let Ok(mut child) = Command::new("clip.exe")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
        else {
            return false;
        };
        if !text.is_ascii() {
            return false;
        }
        let bytes = text.as_bytes();
        let wrote = child
            .stdin
            .take()
            .map(|mut stdin| stdin.write_all(bytes).is_ok())
            .unwrap_or(false);
        let waited = child.wait().map(|s| s.success()).unwrap_or(false);
        wrote && waited
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consent::{Category, Request, Term};

    fn event(kind: &str, category: Option<Category>, step: Step) -> Event {
        Event {
            request: Request {
                kind: kind.into(),
                category,
                ..Request::default()
            },
            step,
        }
    }

    #[test]
    fn jobs_get_short_numbers_in_order() {
        let b = Board::new("http://x/v/n1_a", false);
        assert_eq!(b.begin("c_aa", "hw.cpu"), 1);
        assert_eq!(b.begin("c_bb", "fs.read"), 2);
        assert_eq!(b.number_of("c_bb"), Some(2));
        assert_eq!(b.finish("c_aa", Ending::Done).unwrap().num, 1);
        assert!(b.finish("c_aa", Ending::Done).is_none());
        assert_eq!(b.running().len(), 1);
        let s = b.stats();
        assert_eq!((s.run, s.done), (2, 1));
    }

    #[test]
    fn consent_lines_land_on_the_job_that_asked() {
        console::set_color(false);
        let b = Board::new("l", false);
        b.begin("c1", "fs.read");
        b.begin("c2", "fs.delete");
        b.begin("c3", "fs.delete");
        assert_eq!(b.consent_target("fs.delete", &Step::Asking), Some(2));
        assert_eq!(
            b.consent_target("fs.delete", &Step::Allowed(Term::Once)),
            Some(2)
        );
        assert_eq!(b.consent_target("fs.delete", &Step::Asking), Some(3));
        assert_eq!(b.consent_target("proc.kill", &Step::Asking), None);
    }

    #[test]
    fn consent_lines_say_what_happened() {
        console::set_color(false);
        assert_eq!(
            consent_line(&event(
                "fs.delete",
                Some(Category::Files),
                Step::Allowed(Term::Minutes(30))
            )),
            "You allowed it for the next 30 minutes."
        );
        assert_eq!(
            consent_line(&event(
                "fs.delete",
                Some(Category::Files),
                Step::Allowed(Term::Once)
            )),
            "You allowed it just this once."
        );
        assert_eq!(
            consent_line(&event(
                "tunnel.open",
                Some(Category::Network),
                Step::Allowed(Term::Session)
            )),
            "You allowed it until this session ends."
        );
        assert_eq!(
            consent_line(&event("fs.delete", None, Step::TimedOut)),
            "Nobody answered within 2 min, so Handoff said no for you."
        );
        assert!(consent_line(&event("x", None, Step::Asking)).contains("asking you first"));
    }

    #[test]
    fn presence_lines_follow_the_viewer_count() {
        assert_eq!(presence_line(0, 0), None);
        assert_eq!(presence_line(0, 1).unwrap(), "Someone opened your link.");
        assert!(presence_line(1, 0)
            .unwrap()
            .starts_with("The helper closed the page."));
        assert_eq!(
            presence_line(1, 2).unwrap(),
            "Someone else opened your link. It's open in 2 places now."
        );
        assert_eq!(
            presence_line(2, 1).unwrap(),
            "One page was closed. Your link is still open in 1 place."
        );
    }

    #[test]
    fn presence_waits_for_the_count_to_settle() {
        let b = Board::new("l", false);
        let first = b.see_viewers(1);
        let second = b.see_viewers(0);
        assert_eq!(b.settle_viewers(first), None);
        assert_eq!(b.settle_viewers(second), None);
        let third = b.see_viewers(1);
        assert_eq!(
            b.settle_viewers(third).unwrap(),
            "Someone opened your link."
        );
        assert_eq!(b.settle_viewers(third), None);
    }

    #[test]
    fn refusals_are_told_apart_from_failures() {
        let refused = Outcome {
            ok: false,
            error: ConsentError::Denied.to_string(),
            ..Outcome::default()
        };
        let failed = Outcome {
            ok: false,
            error: "file not found".into(),
            ..Outcome::default()
        };
        let worked = Outcome {
            ok: true,
            ..Outcome::default()
        };
        assert_eq!(ending_for(&refused, false), Ending::Refused);
        assert_eq!(ending_for(&failed, false), Ending::Failed);
        assert_eq!(ending_for(&failed, true), Ending::Stopped);
        assert_eq!(ending_for(&worked, true), Ending::Done);
    }

    #[test]
    fn result_lines_read_plainly() {
        console::set_color(false);
        let mut out = Outcome {
            ok: true,
            elapsed_ms: 1360,
            ..Outcome::default()
        };
        assert_eq!(
            result_line(&out, Ending::Done, false, "AMD Ryzen"),
            "Done in 1.4 s. AMD Ryzen"
        );
        out.ok = false;
        out.error = ConsentError::Denied.to_string();
        assert_eq!(
            result_line(&out, Ending::Refused, true, ""),
            "Not allowed. This session can only look, not make changes."
        );
        assert_eq!(
            result_line(&out, Ending::Refused, false, ""),
            "Not allowed, because you said no."
        );
        assert_eq!(
            result_line(&out, Ending::Failed, false, "file not found"),
            "Failed after 1.4 s: file not found"
        );
        out.elapsed_ms = 3;
        assert_eq!(
            result_line(&out, Ending::Failed, false, "file not found"),
            "Failed: file not found"
        );
    }

    #[test]
    fn exit_summary_counts_every_ending() {
        let stats = Stats {
            run: 3,
            done: 1,
            refused: 1,
            failed: 1,
            stopped: 0,
        };
        assert_eq!(
            exit_summary(stats, Duration::from_secs(65)),
            "The helper ran 3 commands in 1 min 5 s: 1 worked, 1 not allowed, and 1 failed."
        );
        assert_eq!(
            exit_summary(Stats::default(), Duration::from_secs(4)),
            "No commands ran. The session lasted 4.0 s."
        );
        let one = Stats {
            run: 2,
            done: 1,
            ..Stats::default()
        };
        assert_eq!(
            exit_summary(one, Duration::from_secs(30)),
            "The helper ran 2 commands in 30 s: 1 worked and 1 didn't finish."
        );
    }

    fn view(v: serde_json::Value) -> ArgView {
        crate::cmd::summary::describe_args(v.as_object().unwrap())
    }

    #[test]
    fn command_lines_show_number_label_and_kind() {
        console::set_color(false);
        assert_eq!(
            command_lines(1, Some("CPU"), "hw.cpu", &ArgView::default(), 69),
            ["#1  CPU  hw.cpu"]
        );
        assert_eq!(
            command_lines(
                2,
                None,
                "x.y",
                &view(serde_json::json!({"path": "C:\\a"})),
                69
            ),
            ["#2  x.y  path=C:\\a"]
        );
    }

    #[test]
    fn scripts_sit_under_the_header_with_their_line_breaks() {
        console::set_color(false);
        let script = "$a = Get-NetAdapter\r\nforeach ($n in $a) {\r\n\tWrite-Output $n.Name\r\n}";
        let rows = command_lines(
            4,
            Some("Run script"),
            "ps.exec",
            &view(serde_json::json!({ "script": script })),
            69,
        );
        assert_eq!(
            rows,
            [
                "#4  Run script  ps.exec",
                "  \u{2502} $a = Get-NetAdapter",
                "  \u{2502} foreach ($n in $a) {",
                "  \u{2502}     Write-Output $n.Name",
                "  \u{2502} }",
            ]
        );
    }

    #[test]
    fn long_scripts_show_twelve_rows_and_a_count() {
        console::set_color(false);
        let script: String = (1..=30).map(|i| format!("Write-Output {i}\n")).collect();
        let rows = command_lines(
            1,
            Some("Run script"),
            "ps.exec",
            &view(serde_json::json!({ "script": script })),
            69,
        );
        assert_eq!(rows.len(), 1 + 12 + 1);
        assert_eq!(rows[12], "  \u{2502} Write-Output 12");
        assert_eq!(rows[13], "  \u{2502} ... 18 more lines");
    }

    #[test]
    fn long_values_wrap_beside_their_name() {
        console::set_color(false);
        let path = format!("C:\\Users\\ADMIN\\{}\\file.log", "Folder\\".repeat(20));
        let rows = command_lines(
            3,
            Some("Read file"),
            "fs.read",
            &view(serde_json::json!({ "path": path })),
            69,
        );
        assert_eq!(rows[0], "#3  Read file  fs.read");
        assert!(rows[1].starts_with("  path  C:\\Users\\ADMIN\\"));
        for row in &rows[2..] {
            assert!(row.starts_with("        "));
        }
        let joined: String = rows[1..].iter().map(|r| r.replace(' ', "")).collect();
        assert_eq!(joined, format!("path{path}"));
    }

    #[test]
    fn long_results_wrap_under_the_number() {
        console::set_color(false);
        let out = Outcome {
            ok: false,
            elapsed_ms: 1200,
            ..Outcome::default()
        };
        let detail = "Get-WinEvent : There is not an event log on the localhost computer that matches System. At line:1 char:1";
        let rows = result_lines(2, &out, Ending::Failed, false, detail, 40);
        assert!(rows.len() > 2);
        assert!(rows[0].starts_with("#2  Failed after 1.2 s: Get-WinEvent"));
        for row in &rows[1..] {
            assert!(row.starts_with("    ") && !row.starts_with("     "));
        }
        assert!(rows.iter().all(|r| display_width(r) <= 40));
        let joined = rows.iter().map(|r| r.trim()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined, format!("#2  Failed after 1.2 s: {detail}"));
    }

    #[test]
    fn very_long_results_are_capped() {
        console::set_color(false);
        let out = Outcome {
            ok: true,
            elapsed_ms: 50,
            ..Outcome::default()
        };
        let detail = "word ".repeat(200);
        let rows = result_lines(9, &out, Ending::Done, false, &detail, 40);
        assert_eq!(rows.len(), RESULT_ROWS);
        assert!(rows[RESULT_ROWS - 1].ends_with(" ..."));
        assert_eq!(
            result_lines(9, &out, Ending::Done, false, "", 40),
            ["#9  Done in under 0.1 s."]
        );
    }

    #[test]
    fn no_input_makes_a_line_wider_than_the_console() {
        console::set_color(false);
        let huge: String = (1..=2000)
            .map(|i| format!("Write-Output {i}\r\n"))
            .collect();
        let samples = [
            serde_json::json!({ "script": huge }),
            serde_json::json!({ "script": "x".repeat(10_000) }),
            serde_json::json!({ "script": format!("{}\n{}", "网络诊断".repeat(300), "🚀✅🔥".repeat(200)) }),
            serde_json::json!({ "script": "\u{1b}[31mred\u{1b}[0m\r\n\u{1b}]0;title\u{7}x\u{8}\u{202E}y\n\t\t\tz".repeat(50) }),
            serde_json::json!({ "path": "p".repeat(5000), "target": "网".repeat(2000), "url": "u/".repeat(3000) }),
            serde_json::json!({ "k".repeat(500): "v".repeat(500) }),
            serde_json::json!({ "path": "a b", "target": "c", "name": "d", "query": "q", "host": "h", "pid": 1, "key": "k" }),
        ];
        for columns in [40, 80, 120, 400] {
            let width = console::body_width_for(columns);
            for args in &samples {
                let rows = command_lines(
                    123,
                    Some(&"Label ".repeat(30)),
                    "very.long.kind",
                    &view(args.clone()),
                    width,
                );
                for row in &rows {
                    assert!(
                        display_width(row) <= width,
                        "{columns}: {} > {width}: {row}",
                        display_width(row)
                    );
                }
                assert!(rows.len() <= HEADER_ROWS + 3 * (SCRIPT_ROWS + 2));
            }
            let out = Outcome {
                ok: false,
                elapsed_ms: 1,
                ..Outcome::default()
            };
            for detail in [
                "e".repeat(5000),
                "网".repeat(600),
                "a\u{1b}[2Jb ".repeat(100),
            ] {
                let rows = result_lines(7, &out, Ending::Failed, false, &detail, width);
                assert!(rows.len() <= RESULT_ROWS);
                assert!(rows.iter().all(|r| display_width(r) <= width));
                assert!(rows.iter().all(|r| !r.contains('\u{1b}')));
            }
        }
    }
}

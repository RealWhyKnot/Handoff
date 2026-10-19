use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::consent::{self, ConsentError, Event, Step};
use crate::console::{self, human_duration, plural, Tone};
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
        Step::Allowed => format!(
            "You said yes, {}.",
            consent::scope_text(event.request.category)
        ),
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

pub fn result_line(out: &Outcome, ending: Ending, read_only: bool, detail: &str) -> String {
    let took = human_duration(Duration::from_millis(out.elapsed_ms.max(0) as u64));
    match ending {
        Ending::Done => {
            let head = console::paint(Tone::Green, &format!("Done in {took}."));
            if detail.is_empty() {
                head
            } else {
                format!("{head} {detail}")
            }
        }
        Ending::Refused => {
            let why = if read_only {
                "Not allowed. This session can only look, not make changes."
            } else if out.error == ConsentError::TimedOut.to_string() {
                "Not allowed, because nobody answered."
            } else {
                "Not allowed, because you said no."
            };
            console::paint(Tone::Yellow, why)
        }
        Ending::Stopped => console::paint(Tone::Yellow, &format!("Stopped after {took}. {detail}")),
        Ending::Failed => {
            let head = if out.elapsed_ms < 100 {
                "Failed:".to_string()
            } else {
                format!("Failed after {took}:")
            };
            format!("{} {}", console::paint(Tone::Red, &head), detail)
        }
    }
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

pub fn command_line(num: usize, label: Option<&str>, kind: &str, args: &str) -> String {
    let mut text = tag(num);
    text.push_str("  ");
    match label {
        Some(label) => {
            text.push_str(&console::paint(Tone::Cyan, label));
            text.push_str("  ");
            text.push_str(&console::paint(Tone::Dim, kind));
        }
        None => text.push_str(&console::paint(Tone::Cyan, kind)),
    }
    if !args.is_empty() {
        text.push_str("  ");
        text.push_str(args);
    }
    text
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
    use crate::consent::{Category, Request};

    fn event(kind: &str, category: Option<Category>, step: Step) -> Event {
        Event {
            request: Request {
                kind: kind.into(),
                category,
                summary: String::new(),
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
        assert_eq!(b.consent_target("fs.delete", &Step::Allowed), Some(2));
        assert_eq!(b.consent_target("fs.delete", &Step::Asking), Some(3));
        assert_eq!(b.consent_target("proc.kill", &Step::Asking), None);
    }

    #[test]
    fn consent_lines_say_what_happened() {
        console::set_color(false);
        assert_eq!(
            consent_line(&event("fs.delete", Some(Category::Files), Step::Allowed)),
            "You said yes, for the next 15 minutes."
        );
        assert_eq!(
            consent_line(&event(
                "power.restart",
                Some(Category::Power),
                Step::Allowed
            )),
            "You said yes, this one time."
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

    #[test]
    fn command_lines_show_number_label_and_kind() {
        console::set_color(false);
        assert_eq!(
            command_line(1, Some("CPU"), "hw.cpu", ""),
            "#1  CPU  hw.cpu"
        );
        assert_eq!(
            command_line(2, None, "x.y", "path=C:\\a"),
            "#2  x.y  path=C:\\a"
        );
    }
}

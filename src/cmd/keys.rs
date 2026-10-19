use std::io::BufRead;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::host::{self, Board};
use super::pause::stdin_is_terminal;
use crate::audit::Logger;
use crate::capabilities;
use crate::console::{self, human_duration, plural};

pub const ENDING: &str = "Ending the session...";

pub fn start_keyboard_control(
    cancel: CancellationToken,
    log: Option<Arc<Logger>>,
    board: Arc<Board>,
) {
    if !stdin_is_terminal() {
        return;
    }

    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if cancel.is_cancelled() {
                return;
            }

            match line.trim().to_lowercase().as_str() {
                "q" | "quit" | "exit" => {
                    console::line(ENDING);
                    cancel.cancel();
                    return;
                }
                "c" => copy_link(&board),
                "s" => print_running(&board),
                "p" => print_grants(),
                "r" => {
                    let n = capabilities::ledger().revoke_all();
                    console::line(&revoke_text(n));
                }
                "a" => match &log {
                    Some(log) => console::line(&format!(
                        "Handoff keeps a record of this session in {}",
                        log.dir().display()
                    )),
                    None => console::line("Handoff couldn't save a record of this session."),
                },
                "" => {}
                _ => print_key_help(),
            }
        }
    });
}

fn copy_link(board: &Board) {
    if host::copy_to_clipboard(board.link()) {
        console::line("The link is copied again.");
    } else {
        console::line(&format!(
            "Handoff couldn't copy the link. Here it is: {}",
            board.link()
        ));
    }
}

pub fn revoke_text(n: usize) -> String {
    if n == 0 {
        return "You hadn't allowed anything, so there was nothing to take back.".to_string();
    }
    format!(
        "Took back {}. The helper has to ask again before making changes.",
        plural(n, "permission", "permissions")
    )
}

fn print_running(board: &Board) {
    let running = board.running();
    if running.is_empty() {
        console::line("Nothing is running right now.");
        return;
    }
    console::line("Running now:");
    for job in running {
        let label = board.label(&job.kind).unwrap_or_else(|| job.kind.clone());
        println!(
            "          {}  {label}  {}",
            host::tag(job.num),
            console::paint(
                console::Tone::Dim,
                &format!("for {}", human_duration(job.started.elapsed()))
            )
        );
    }
}

pub fn round_to_minute(secs: f64) -> std::time::Duration {
    std::time::Duration::from_secs((secs / 60.0).round() as u64 * 60)
}

pub fn expiry_text(remaining_secs: Option<f64>) -> String {
    match remaining_secs {
        None => "until this session ends".to_string(),
        Some(secs) => {
            let rounded = round_to_minute(secs);
            if rounded.is_zero() {
                "less than a minute left".to_string()
            } else {
                format!("{} left", human_duration(rounded))
            }
        }
    }
}

fn print_grants() {
    let grants = capabilities::ledger().grants();
    if grants.is_empty() {
        console::line("You haven't allowed any changes.");
        return;
    }
    console::line("You've allowed the helper to:");
    for g in grants {
        let remaining = match g.expires_at {
            Some(at) if g.expires() => {
                Some(at.saturating_duration_since(Instant::now()).as_secs_f64())
            }
            _ => None,
        };
        println!(
            "          {} ({})",
            g.category.label(),
            expiry_text(remaining)
        );
    }
}

pub fn key_help() -> String {
    [
        "Type a letter and press Enter:",
        "  q  end the session",
        "  c  copy the link again",
        "  s  show what's running",
        "  p  show what you've allowed",
        "  r  take back everything you've allowed",
        "  a  show where the record of this session is kept",
        "  ?  show this list",
    ]
    .join("\n")
}

fn print_key_help() {
    println!("{}", key_help());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_rounds_to_whole_minutes() {
        assert_eq!(expiry_text(Some(899.0)), "15 min left");
        assert_eq!(expiry_text(Some(59.0)), "1 min left");
        assert_eq!(expiry_text(Some(20.0)), "less than a minute left");
        assert_eq!(expiry_text(Some(3600.0)), "1 h left");
        assert_eq!(expiry_text(None), "until this session ends");
    }

    #[test]
    fn help_lists_every_key() {
        let help = key_help();
        for key in [
            "q  end", "c  copy", "s  show", "p  show", "r  take", "a  show", "?  show",
        ] {
            assert!(help.contains(key), "{key}");
        }
    }

    #[test]
    fn revoke_text_counts_permissions() {
        assert!(revoke_text(0).contains("nothing to take back"));
        assert!(revoke_text(1).starts_with("Took back 1 permission."));
        assert!(revoke_text(2).starts_with("Took back 2 permissions."));
    }
}

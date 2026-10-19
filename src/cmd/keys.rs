use std::io::BufRead;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::pause::stdin_is_terminal;
use crate::audit::Logger;
use crate::capabilities;
use crate::util;

pub fn start_keyboard_control(cancel: CancellationToken, log: Option<Arc<Logger>>) {
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
                    println!("shutting down...");
                    cancel.cancel();
                    return;
                }
                "p" => print_grants(),
                "r" => {
                    let n = capabilities::ledger().revoke_all();
                    println!("revoked {n} permission(s); the next request will ask again");
                }
                "a" => match &log {
                    Some(log) => println!("audit log: {}", log.dir().display()),
                    None => println!("audit log unavailable for this session"),
                },
                "" => {}
                _ => print_key_help(),
            }
        }
    });
}

pub fn round_to_minute(secs: f64) -> std::time::Duration {
    std::time::Duration::from_secs((secs / 60.0).round() as u64 * 60)
}

fn print_grants() {
    let grants = capabilities::ledger().grants();
    if grants.is_empty() {
        println!("nothing is approved right now");
        return;
    }
    println!("approved right now:");
    for g in grants {
        let name = g.category.as_str();
        match g.expires_at {
            Some(at) if g.expires() => {
                let remaining = at.saturating_duration_since(Instant::now());
                let rounded = round_to_minute(remaining.as_secs_f64());
                println!(
                    "  {name:<10} expires in {}",
                    util::go_duration_string(rounded)
                );
            }
            _ => println!("  {name:<10} for this session"),
        }
    }
}

pub fn key_help() -> &'static str {
    "keys: q quit  p show what is approved  r revoke approvals  a audit log location  ? this list"
}

fn print_key_help() {
    println!("{}", key_help());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_rounds_to_whole_minutes() {
        assert_eq!(util::go_duration_string(round_to_minute(899.0)), "15m0s");
        assert_eq!(util::go_duration_string(round_to_minute(59.0)), "1m0s");
        assert_eq!(util::go_duration_string(round_to_minute(20.0)), "0s");
        assert_eq!(util::go_duration_string(round_to_minute(3600.0)), "1h0m0s");
    }

    #[test]
    fn help_line_lists_every_key() {
        for key in ["q quit", "p show", "r revoke", "a audit", "? this list"] {
            assert!(key_help().contains(key));
        }
    }
}

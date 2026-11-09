use crate::dispatch::Progress;
use crate::relay::Bridge;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const PROGRESS_FLUSH: Duration = Duration::from_millis(250);

pub(super) const PROGRESS_DRAIN: Duration = Duration::from_secs(2);

const PROGRESS_FRAME_CAP: usize = 16 * 1024;

const PROGRESS_TOTAL_CAP: usize = 1024 * 1024;

const PROGRESS_STOPPED: &str =
    "\nLive output stopped at 1 MB. The full output arrives with the result.\n";

pub(super) fn progress_forwarder(bridge: Bridge, id: String) -> (Progress, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let progress: Progress = Arc::new(move |text| {
        let _ = tx.send(text);
    });
    let task = tokio::spawn(async move {
        let mut sent = 0usize;
        while let Some(first) = rx.recv().await {
            let mut batch = first;
            let deadline = tokio::time::Instant::now() + PROGRESS_FLUSH;
            while batch.len() < PROGRESS_FRAME_CAP {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(more)) => batch.push_str(&more),
                    _ => break,
                }
            }
            if sent >= PROGRESS_TOTAL_CAP {
                continue;
            }
            let room = PROGRESS_TOTAL_CAP - sent;
            let over = batch.len() > room;
            truncate_at_char(&mut batch, room);
            sent += batch.len();
            if over {
                batch.push_str(PROGRESS_STOPPED);
                sent = PROGRESS_TOTAL_CAP;
            }
            for piece in split_at_chars(&batch, PROGRESS_FRAME_CAP) {
                if let Err(err) = bridge.send_command_progress(&id, piece).await {
                    tracing::info!("send progress failed id={id}: {err}");
                    break;
                }
            }
        }
    });
    (progress, task)
}

pub(super) fn truncate_at_char(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

pub(super) fn split_at_chars(s: &str, max: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = s;
    while rest.len() > max {
        let mut end = max;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (head, tail) = rest.split_at(end);
        pieces.push(head);
        rest = tail;
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

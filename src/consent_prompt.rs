use super::consent::Answer;
use super::consent::ConsentError;
use super::consent::PROMPT_CLOSE_WAIT;
use super::consent::PROMPT_TIMEOUT;
use super::consent::Prompt;
use super::consent::Request;
use super::consent_policy::Category;
use super::consent_policy::Term;
use crate::consent::dialog;
use crate::dispatch::for_display;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const SUMMARY_CHARS: usize = 2000;

const SUMMARY_LINES: usize = 40;

const DETAIL_CHARS: usize = 200_000;

pub(super) const DETAIL_LINES: usize = 20_000;

pub(super) const CUT_NOTE: &str = "(The rest is too long to show here.)";

pub const PROMPT_TITLE: &str = "Allow this Handoff request?";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptParts {
    pub instruction: String,
    pub summary: String,
    pub scope: String,
    pub caution: String,
    pub command: Option<String>,
    pub detail: Option<String>,
    pub grant_button: Option<String>,
}

pub fn prompt_parts(req: &Request) -> PromptParts {
    let shown = for_display(&req.summary, SUMMARY_CHARS, SUMMARY_LINES);
    let text = shown.text.trim();
    let (head, command) = match text.split_once("\n\n") {
        Some((head, rest)) => (head.trim().to_string(), Some(rest.trim().to_string())),
        None if text.contains('\n') => (String::new(), Some(text.to_string())),
        None => (text.to_string(), None),
    };
    let mut command = command.filter(|c| !c.is_empty());
    if shown.cut {
        match &mut command {
            Some(c) => {
                c.push('\n');
                c.push_str(CUT_NOTE);
            }
            None => command = Some(CUT_NOTE.to_string()),
        }
    }
    let detail = match &req.detail {
        Some(full) => {
            let shown = for_display(full, DETAIL_CHARS, DETAIL_LINES);
            let mut text = shown.text;
            if shown.cut {
                text.push('\n');
                text.push_str(CUT_NOTE);
            }
            Some(text).filter(|t| !t.is_empty())
        }
        None => command.clone(),
    };
    let head = if head.is_empty() && command.is_none() {
        "Run a command that can change this computer.".to_string()
    } else {
        head
    };
    let term = req.term();
    let granted = req
        .category
        .map(Category::label)
        .unwrap_or("run this kind of command");
    let scope = match term {
        Term::Once => "If you allow it, only this request goes ahead. They have to ask again for anything else."
            .to_string(),
        term => format!(
            "If you allow it, they can {granted} {}. For any other kind of change they have to ask you again.",
            term.text()
        ),
    };
    PromptParts {
        instruction: "The person helping you wants to make a change on this computer".to_string(),
        summary: head,
        scope,
        caution: "Only allow it if you know this person and asked them for help.".to_string(),
        command,
        detail,
        grant_button: term.button(),
    }
}

impl PromptParts {
    pub fn scope_line(&self) -> String {
        let mut out = self.scope.clone();
        if self.grant_button.is_some() {
            out.push_str(" Choose Just this once to allow only this request.");
        }
        out
    }
}

pub fn countdown_text(left: Duration) -> String {
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    format!(
        "If you don't answer, Handoff says no for you in {}:{:02}.",
        secs / 60,
        secs % 60
    )
}

pub fn prompt_text(req: &Request) -> String {
    let parts = prompt_parts(req);
    let mut out = format!("{}.\n\n", parts.instruction);
    for piece in [Some(&parts.summary), parts.command.as_ref()]
        .into_iter()
        .flatten()
    {
        if !piece.is_empty() {
            out.push_str(piece);
            out.push_str("\n\n");
        }
    }
    out.push_str(&parts.scope);
    out.push_str("\n\n");
    out.push_str(&parts.caution);
    let _ = write!(
        out,
        " If you don't answer within {} minutes, Handoff says no for you.",
        PROMPT_TIMEOUT.as_secs() / 60
    );
    out
}

pub fn system_prompt() -> Prompt {
    Arc::new(|cancel, req| Box::pin(system_prompt_impl(cancel, req)))
}

#[cfg(windows)]
async fn system_prompt_impl(
    cancel: CancellationToken,
    req: Request,
) -> Result<Answer, ConsentError> {
    use std::sync::atomic::{AtomicU32, Ordering};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }

    if cancel.is_cancelled() {
        return Err(ConsentError::Cancelled);
    }
    let thread = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&thread);
    let shown = req.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        // SAFETY: GetCurrentThreadId takes no arguments and cannot fail.
        seen.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        dialog::show(&shown, &dialog::Options::default())
    });
    tokio::select! {
        joined = &mut task => {
            joined.map_err(|e| ConsentError::Other(format!("consent prompt failed: {e}")))?
        }
        _ = cancel.cancelled() => {
            let deadline = Instant::now() + PROMPT_CLOSE_WAIT;
            let mut closed = false;
            loop {
                let id = thread.load(Ordering::SeqCst);
                if id != 0 {
                    dialog::close(id);
                }
                match tokio::time::timeout(Duration::from_millis(100), &mut task).await {
                    Ok(_) => {
                        closed = true;
                        break;
                    }
                    Err(_) if Instant::now() >= deadline => break,
                    Err(_) => {}
                }
            }
            tracing::info!("consent prompt withdrawn kind={} closed={closed}", req.kind);
            Err(ConsentError::Cancelled)
        }
    }
}

#[cfg(not(windows))]
async fn system_prompt_impl(
    cancel: CancellationToken,
    req: Request,
) -> Result<Answer, ConsentError> {
    if cancel.is_cancelled() {
        return Err(ConsentError::Cancelled);
    }
    let term = req.term();
    crate::errln!("{}", prompt_text(&req));
    crate::err!("{}", typed_question(term));
    let read = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => Err(ConsentError::Other("no answer could be read".into())),
            Ok(_) => Ok(typed_answer(&line, term)),
            Err(e) => Err(ConsentError::Other(e.to_string())),
        }
    });
    tokio::select! {
        joined = read => joined.map_err(|e| ConsentError::Other(e.to_string()))?,
        _ = cancel.cancelled() => Err(ConsentError::Cancelled),
    }
}

pub fn typed_question(term: Term) -> String {
    match term {
        Term::Once => {
            "Type y and press Enter to allow it. Press Enter on its own to say no: ".to_string()
        }
        term => format!(
            "Type y and press Enter to allow it {}, or o to allow just this once. Press Enter on its own to say no: ",
            term.text()
        ),
    }
}

pub fn typed_answer(line: &str, term: Term) -> Answer {
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" if term == Term::Once => Answer::Once,
        "y" | "yes" => Answer::ForGrant,
        "o" | "once" => Answer::Once,
        _ => Answer::No,
    }
}

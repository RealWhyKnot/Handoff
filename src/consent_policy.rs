use super::consent::EXEC_GRANT_TTL;
use super::consent::FILES_GRANT_TTL;
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Files,
    Processes,
    Services,
    Devices,
    Network,
    Capture,
    Exec,
    Power,
}

impl Category {
    pub const ALL: [Category; 8] = [
        Category::Files,
        Category::Processes,
        Category::Services,
        Category::Devices,
        Category::Network,
        Category::Capture,
        Category::Exec,
        Category::Power,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Files => "files",
            Category::Processes => "processes",
            Category::Services => "services",
            Category::Devices => "devices",
            Category::Network => "network",
            Category::Capture => "capture",
            Category::Exec => "exec",
            Category::Power => "power",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::Files => "read, add, change, and delete your files",
            Category::Processes => "start and stop programs",
            Category::Services => "start and stop Windows services",
            Category::Devices => "reprogram attached devices such as a Pico board",
            Category::Network => {
                "open connections to this PC and to devices on your local network such as your router"
            }
            Category::Capture => "capture your screen and set your clipboard",
            Category::Exec => "run PowerShell commands",
            Category::Power => "restart or shut down this computer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    NotRequired,
    PromptAllow,
    PromptAllowOnce,
    PromptAllowFor(u32),
    PromptDeny,
    GrantCached,
    GrantExpired,
}

impl Decision {
    pub fn as_str(self) -> Cow<'static, str> {
        match self {
            Decision::NotRequired => "not_required".into(),
            Decision::PromptAllow => "prompt_allow".into(),
            Decision::PromptAllowOnce => "prompt_allow_once".into(),
            Decision::PromptAllowFor(minutes) => format!("prompt_allow_{minutes}m").into(),
            Decision::PromptDeny => "prompt_deny".into(),
            Decision::GrantCached => "grant_cached".into(),
            Decision::GrantExpired => "grant_expired".into(),
        }
    }
}

pub const CONSENT_ARG: &str = "consent_minutes";

pub const MAX_CONSENT_MINUTES: u32 = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ask {
    #[default]
    Default,
    Once,
    Minutes(u32),
}

pub fn take_ask(kind: &str, args: &mut Map<String, Value>) -> Result<Ask, String> {
    let ask = match args.remove(CONSENT_ARG) {
        None | Some(Value::Null) => return Ok(Ask::Default),
        Some(v) => v.as_u64(),
    };
    match ask.map(u32::try_from) {
        Some(Ok(0)) => Ok(Ask::Once),
        Some(Ok(n)) if n <= MAX_CONSENT_MINUTES => Ok(Ask::Minutes(n)),
        _ => Err(format!(
            "{kind}: '{CONSENT_ARG}' must be a whole number from 0 to {MAX_CONSENT_MINUTES} (0 means just this once)"
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Term {
    Once,
    Minutes(u32),
    Session,
}

impl Term {
    pub fn length(self) -> Option<Duration> {
        match self {
            Term::Minutes(m) => Some(Duration::from_secs(u64::from(m) * 60)),
            _ => None,
        }
    }

    pub fn text(self) -> String {
        match self {
            Term::Once => "just this once".to_string(),
            Term::Minutes(m) => format!("for the next {}", minutes_text(m, true)),
            Term::Session => "until this session ends".to_string(),
        }
    }

    pub fn button(self) -> Option<String> {
        match self {
            Term::Once => None,
            Term::Minutes(m) => Some(format!("Allow for {}", minutes_text(m, false))),
            Term::Session => Some("Allow until this session ends".to_string()),
        }
    }
}

fn minutes_text(m: u32, after_next: bool) -> String {
    match (m, m % 60) {
        (1, _) if after_next => "minute".to_string(),
        (1, _) => "1 minute".to_string(),
        (60, _) if after_next => "hour".to_string(),
        (60, _) => "1 hour".to_string(),
        (m, 0) => format!("{} hours", m / 60),
        (m, _) => format!("{m} minutes"),
    }
}

pub fn term_for(category: Category, ask: Ask) -> Term {
    if category == Category::Power {
        return Term::Once;
    }
    match ask {
        Ask::Once => Term::Once,
        Ask::Minutes(m) => Term::Minutes(m),
        Ask::Default => match grant_ttl(category) {
            Some(ttl) if ttl.is_zero() => Term::Once,
            Some(ttl) => Term::Minutes(u32::try_from(ttl.as_secs() / 60).unwrap_or(u32::MAX)),
            None => Term::Session,
        },
    }
}

pub const GATED_KINDS: [(&str, Category); 18] = [
    ("fs.upload", Category::Files),
    ("fs.mkdir", Category::Files),
    ("fs.delete", Category::Files),
    ("fs.write", Category::Files),
    ("pico.save", Category::Files),
    ("proc.kill", Category::Processes),
    ("proc.start", Category::Processes),
    ("svc.control", Category::Services),
    ("pico.bootsel", Category::Devices),
    ("pico.flash", Category::Devices),
    ("pico.reset", Category::Devices),
    ("tunnel.open", Category::Network),
    ("net.repair", Category::Network),
    ("sys.screenshot", Category::Capture),
    ("clip.set", Category::Capture),
    ("ps.exec", Category::Exec),
    ("sys.restart", Category::Power),
    ("sys.shutdown", Category::Power),
];

pub fn category_for(kind: &str) -> Option<Category> {
    GATED_KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, c)| *c)
}

pub fn grant_ttl(category: Category) -> Option<Duration> {
    match category {
        Category::Exec => Some(EXEC_GRANT_TTL),
        Category::Files => Some(FILES_GRANT_TTL),
        Category::Power => Some(Duration::ZERO),
        _ => None,
    }
}

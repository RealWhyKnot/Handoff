use std::path::PathBuf;

use super::flags::Options;
use crate::{audit, supportlog};

pub fn support_log_path(opts: Option<&Options>) -> String {
    if let Some(opts) = opts {
        if !opts.log_dir.is_empty() {
            return PathBuf::from(&opts.log_dir)
                .join("handoff.log")
                .display()
                .to_string();
        }
    }
    supportlog::path()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

pub fn audit_dir(opts: Option<&Options>) -> String {
    if let Some(opts) = opts {
        if !opts.audit_dir.is_empty() {
            return opts.audit_dir.clone();
        }
    }
    audit::default_dir().display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_dirs_win() {
        let opts = Options {
            log_dir: "C:\\logs".into(),
            audit_dir: "C:\\audit".into(),
            ..Options::default()
        };
        assert!(support_log_path(Some(&opts)).ends_with("handoff.log"));
        assert!(support_log_path(Some(&opts)).starts_with("C:\\logs"));
        assert_eq!(audit_dir(Some(&opts)), "C:\\audit");
    }

    #[test]
    fn defaults_point_under_whyknot() {
        let dir = audit_dir(None);
        assert!(dir.contains("whyknot"), "{dir}");
        assert!(dir.ends_with("audit"), "{dir}");
        assert!(support_log_path(None).ends_with("handoff.log"));
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::util;

pub const DEFAULT_RELAY_URL: &str = "https://handoff.whyknot.dev";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Str,
    Bool,
    Duration,
    List,
}

#[derive(Debug, Clone, Copy)]
pub struct Def {
    pub name: &'static str,
    pub key: &'static str,
    pub kind: Kind,
    pub usage: &'static str,
    pub default: &'static str,
}

impl Def {
    pub const fn string(name: &'static str, usage: &'static str, default: &'static str) -> Def {
        Def {
            name,
            key: name,
            kind: Kind::Str,
            usage,
            default,
        }
    }

    pub const fn boolean(name: &'static str, usage: &'static str) -> Def {
        Def {
            name,
            key: name,
            kind: Kind::Bool,
            usage,
            default: "false",
        }
    }

    pub const fn duration(name: &'static str, usage: &'static str, default: &'static str) -> Def {
        Def {
            name,
            key: name,
            kind: Kind::Duration,
            usage,
            default,
        }
    }

    pub const fn list(name: &'static str, usage: &'static str) -> Def {
        Def {
            name,
            key: name,
            kind: Kind::List,
            usage,
            default: "",
        }
    }

    pub const fn alias_of(mut self, key: &'static str) -> Def {
        self.key = key;
        self
    }

    fn is_zero_default(&self) -> bool {
        match self.kind {
            Kind::Str | Kind::List => self.default.is_empty(),
            Kind::Bool => self.default == "false",
            Kind::Duration => util::parse_go_duration(self.default)
                .map(|d| d.is_zero())
                .unwrap_or(true),
        }
    }
}

pub const SHARED: [Def; 7] = [
    Def::string("relay", "relay base URL", ""),
    Def::boolean("json", "print machine-readable output"),
    Def::boolean("quiet", "print less"),
    Def::boolean("verbose", "print more"),
    Def::string("log-dir", "directory for the support log", ""),
    Def::string("audit-dir", "directory for the audit log", ""),
    Def::string("config", "path to a config file", ""),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagError {
    Help,
    Message(String),
}

impl fmt::Display for FlagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlagError::Help => f.write_str("flag: help requested"),
            FlagError::Message(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for FlagError {}

#[derive(Debug, Clone, Default)]
pub struct Parsed {
    values: BTreeMap<&'static str, Vec<String>>,
    defaults: BTreeMap<&'static str, &'static str>,
    seen: BTreeSet<String>,
    pub args: Vec<String>,
}

impl Parsed {
    pub fn string(&self, key: &str) -> String {
        self.values
            .get(key)
            .and_then(|v| v.last().cloned())
            .unwrap_or_else(|| self.defaults.get(key).copied().unwrap_or("").to_string())
    }

    pub fn boolean(&self, key: &str) -> bool {
        self.values
            .get(key)
            .and_then(|v| v.last())
            .map(|v| parse_go_bool(v).unwrap_or(false))
            .unwrap_or(false)
    }

    pub fn duration(&self, key: &str) -> Duration {
        let raw = self.string(key);
        util::parse_go_duration(&raw).unwrap_or_default()
    }

    pub fn list(&self, key: &str) -> Vec<String> {
        self.values.get(key).cloned().unwrap_or_default()
    }

    pub fn was_set(&self, name: &str) -> bool {
        self.seen.contains(name)
    }
}

pub fn parse_go_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct FlagSet {
    name: String,
    defs: Vec<Def>,
}

impl FlagSet {
    pub fn new(name: &str, extra: &[Def]) -> FlagSet {
        let mut defs: Vec<Def> = SHARED.to_vec();
        defs.extend_from_slice(extra);
        FlagSet {
            name: name.to_string(),
            defs,
        }
    }

    pub fn defs(&self) -> &[Def] {
        &self.defs
    }

    fn lookup(&self, name: &str) -> Option<&Def> {
        self.defs.iter().find(|d| d.name == name)
    }

    pub fn usage_text(&self) -> String {
        let mut out = format!("Usage of {}:\n", self.name);
        let mut sorted: Vec<&Def> = self.defs.iter().collect();
        sorted.sort_by_key(|d| d.name);
        for d in sorted {
            let mut line = format!("  -{}", d.name);
            let type_name = match d.kind {
                Kind::Bool => "",
                Kind::Str => "string",
                Kind::Duration => "duration",
                Kind::List => "value",
            };
            if !type_name.is_empty() {
                line.push(' ');
                line.push_str(type_name);
            }
            if line.len() <= 4 {
                line.push('\t');
            } else {
                line.push_str("\n    \t");
            }
            line.push_str(&d.usage.replace('\n', "\n    \t"));
            if !d.is_zero_default() {
                if d.kind == Kind::Str {
                    line.push_str(&format!(" (default {})", util::go_quote(d.default)));
                } else {
                    line.push_str(&format!(" (default {})", d.default));
                }
            }
            line.push('\n');
            out.push_str(&line);
        }
        out
    }

    fn fail(&self, message: String) -> FlagError {
        println!("{message}");
        print!("{}", self.usage_text());
        FlagError::Message(message)
    }

    pub fn reorder_flags_first(&self, args: &[String]) -> Vec<String> {
        let takes_value = |name: &str| {
            self.lookup(name)
                .map(|d| d.kind != Kind::Bool)
                .unwrap_or(false)
        };

        let mut flags: Vec<String> = Vec::new();
        let mut positional: Vec<String> = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a == "--" {
                positional.extend_from_slice(&args[i + 1..]);
                break;
            }
            if !a.starts_with('-') || a == "-" {
                positional.push(a.clone());
                i += 1;
                continue;
            }

            flags.push(a.clone());
            let name = a.trim_start_matches('-');
            if name.contains('=') {
                i += 1;
                continue;
            }
            if takes_value(name) && i + 1 < args.len() {
                i += 1;
                flags.push(args[i].clone());
            }
            i += 1;
        }
        flags.extend(positional);
        flags
    }

    pub fn parse(&self, args: &[String]) -> Result<Parsed, FlagError> {
        let mut parsed = Parsed::default();
        for d in &self.defs {
            parsed.defaults.insert(d.key, d.default);
        }

        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg.len() < 2 || !arg.starts_with('-') {
                break;
            }
            i += 1;
            let mut minuses = 1;
            if arg.as_bytes()[1] == b'-' {
                minuses = 2;
                if arg.len() == 2 {
                    break;
                }
            }
            let body = &arg[minuses..];
            if body.is_empty() || body.starts_with('-') || body.starts_with('=') {
                return Err(self.fail(format!("bad flag syntax: {arg}")));
            }
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };

            let Some(def) = self.lookup(name).copied() else {
                if name == "help" || name == "h" {
                    print!("{}", self.usage_text());
                    return Err(FlagError::Help);
                }
                return Err(self.fail(format!("flag provided but not defined: -{name}")));
            };

            let value = if def.kind == Kind::Bool {
                match inline {
                    Some(v) => {
                        if parse_go_bool(&v).is_none() {
                            return Err(self.fail(format!(
                                "invalid boolean value {} for -{name}: parse error",
                                util::go_quote(&v)
                            )));
                        }
                        v
                    }
                    None => "true".to_string(),
                }
            } else {
                let v = match inline {
                    Some(v) => v,
                    None if i < args.len() => {
                        i += 1;
                        args[i - 1].clone()
                    }
                    None => {
                        return Err(self.fail(format!("flag needs an argument: -{name}")));
                    }
                };
                if def.kind == Kind::Duration && util::parse_go_duration(&v).is_none() {
                    return Err(self.fail(format!(
                        "invalid value {} for flag -{name}: parse error",
                        util::go_quote(&v)
                    )));
                }
                v
            };

            parsed.values.entry(def.key).or_default().push(value);
            parsed.seen.insert(name.to_string());
        }

        parsed.args = args[i.min(args.len())..].to_vec();
        Ok(parsed)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub relay: String,
    pub json: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub log_dir: String,
    pub audit_dir: String,
    pub config: String,
    pub keep_awake: bool,
    pub relay_set: bool,
}

#[derive(Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    relay: String,
    #[serde(default)]
    log_dir: String,
    #[serde(default)]
    audit_dir: String,
    #[serde(default)]
    keep_awake: Option<bool>,
}

pub type Env<'a> = &'a dyn Fn(&str) -> String;

fn trimmed_first(values: &[&str]) -> String {
    util::first_non_empty(values)
}

impl Options {
    pub fn resolve(&mut self, env: Env<'_>) -> Result<(), String> {
        let cfg = load_config(&self.config, env);

        if self.relay.is_empty() {
            self.relay = trimmed_first(&[&env("HANDOFF_RELAY"), &cfg.relay, DEFAULT_RELAY_URL]);
        }
        if self.log_dir.is_empty() {
            self.log_dir = trimmed_first(&[&env("HANDOFF_LOG_DIR"), &cfg.log_dir]);
        }
        if self.audit_dir.is_empty() {
            self.audit_dir = trimmed_first(&[&env("HANDOFF_AUDIT_DIR"), &cfg.audit_dir]);
        }

        self.keep_awake = cfg.keep_awake.unwrap_or(true);

        self.relay = self.relay.trim().trim_end_matches('/').to_string();
        let valid = url::Url::parse(&self.relay)
            .map(|u| {
                u.host_str().map(|h| !h.is_empty()).unwrap_or(false)
                    && (u.scheme() == "http" || u.scheme() == "https")
            })
            .unwrap_or(false);
        if !valid {
            return Err(format!(
                "relay must be an http or https URL, got {}",
                util::go_quote(&self.relay)
            ));
        }
        Ok(())
    }
}

fn load_config(explicit: &str, env: Env<'_>) -> FileConfig {
    let mut path = explicit.to_string();
    if path.is_empty() {
        path = env("HANDOFF_CONFIG");
    }
    let mut path_buf: Option<PathBuf> = (!path.is_empty()).then(|| PathBuf::from(&path));
    if path_buf.is_none() {
        path_buf = util::user_config_dir_from(env)
            .map(|dir| dir.join("whyknot").join("handoff").join("config.json"));
    }
    let Some(path) = path_buf else {
        return FileConfig::default();
    };

    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(err) => {
            if !explicit.is_empty() && err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("warning: could not read config {}: {err}", path.display());
            }
            return FileConfig::default();
        }
    };

    match serde_json::from_slice::<Option<FileConfig>>(&data) {
        Ok(cfg) => cfg.unwrap_or_default(),
        Err(err) => {
            eprintln!(
                "warning: ignoring malformed config {}: {err}",
                path.display()
            );
            FileConfig::default()
        }
    }
}

pub fn parse_options(
    name: &str,
    args: &[String],
    extra: &[Def],
) -> Result<(Options, Parsed), FlagError> {
    parse_options_with(name, args, extra, &util::env_var)
}

pub fn parse_options_with(
    name: &str,
    args: &[String],
    extra: &[Def],
    env: Env<'_>,
) -> Result<(Options, Parsed), FlagError> {
    let set = FlagSet::new(name, extra);
    let parsed = set.parse(&set.reorder_flags_first(args))?;

    let mut opts = Options {
        relay: parsed.string("relay"),
        json: parsed.boolean("json"),
        quiet: parsed.boolean("quiet"),
        verbose: parsed.boolean("verbose"),
        log_dir: parsed.string("log-dir"),
        audit_dir: parsed.string("audit-dir"),
        config: parsed.string("config"),
        keep_awake: true,
        relay_set: parsed.was_set("relay"),
    };

    if opts.relay_set && opts.relay.trim().is_empty() {
        return Err(FlagError::Message(
            "--relay was given but empty".to_string(),
        ));
    }

    opts.resolve(env).map_err(FlagError::Message)?;
    Ok((opts, parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> String {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned().unwrap_or_default()
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn config_file(dir: &tempfile::TempDir, name: &str, body: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        path.display().to_string()
    }

    #[test]
    fn relay_precedence_flag_beats_env_beats_config_beats_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_file(&dir, "config.json", r#"{"relay":"https://config.example"}"#);

        let env = env_of(&[("HANDOFF_RELAY", ""), ("HANDOFF_CONFIG", "")]);
        let (opts, _) = parse_options_with("test", &[], &[], &env).unwrap();
        assert_eq!(opts.relay, DEFAULT_RELAY_URL);

        let env = env_of(&[("HANDOFF_RELAY", ""), ("HANDOFF_CONFIG", &cfg)]);
        let (opts, _) = parse_options_with("test", &[], &[], &env).unwrap();
        assert_eq!(opts.relay, "https://config.example");

        let env = env_of(&[
            ("HANDOFF_RELAY", "https://env.example"),
            ("HANDOFF_CONFIG", &cfg),
        ]);
        let (opts, _) = parse_options_with("test", &[], &[], &env).unwrap();
        assert_eq!(opts.relay, "https://env.example");

        let (opts, _) = parse_options_with(
            "test",
            &strs(&["--relay", "https://flag.example"]),
            &[],
            &env,
        )
        .unwrap();
        assert_eq!(opts.relay, "https://flag.example");
    }

    #[test]
    fn trailing_slash_is_trimmed_from_relay() {
        let env = env_of(&[]);
        let (opts, _) = parse_options_with(
            "test",
            &strs(&["--relay", "https://example.test/"]),
            &[],
            &env,
        )
        .unwrap();
        assert_eq!(opts.relay, "https://example.test");
    }

    #[test]
    fn invalid_relay_is_a_usage_error() {
        let env = env_of(&[]);
        for bad in ["not-a-url", "ftp://example.test", ""] {
            assert!(
                parse_options_with("test", &strs(&["--relay", bad]), &[], &env).is_err(),
                "relay {bad:?} was accepted"
            );
        }
    }

    #[test]
    fn empty_relay_flag_has_its_own_message() {
        let env = env_of(&[]);
        let err = parse_options_with("test", &strs(&["--relay", ""]), &[], &env).unwrap_err();
        assert_eq!(
            err,
            FlagError::Message("--relay was given but empty".to_string())
        );
    }

    #[test]
    fn bad_relay_message_names_the_value() {
        let env = env_of(&[]);
        let err = parse_options_with("test", &strs(&["--relay", "ftp://example.test"]), &[], &env)
            .unwrap_err();
        assert_eq!(
            err,
            FlagError::Message(
                "relay must be an http or https URL, got \"ftp://example.test\"".to_string()
            )
        );
    }

    #[test]
    fn malformed_config_warns_but_does_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_file(&dir, "broken.json", "{not json");
        let env = env_of(&[("HANDOFF_RELAY", ""), ("HANDOFF_CONFIG", &cfg)]);
        let (opts, _) = parse_options_with("test", &[], &[], &env).unwrap();
        assert_eq!(opts.relay, DEFAULT_RELAY_URL);
    }

    #[test]
    fn missing_config_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.json").display().to_string();
        let env = env_of(&[("HANDOFF_RELAY", ""), ("HANDOFF_CONFIG", &absent)]);
        assert!(parse_options_with("test", &[], &[], &env).is_ok());
    }

    #[test]
    fn config_supplies_dirs_and_keep_awake() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_file(
            &dir,
            "config.json",
            r#"{"log_dir":"C:\\logs","audit_dir":"C:\\audit","keep_awake":false}"#,
        );
        let env = env_of(&[("HANDOFF_CONFIG", &cfg)]);
        let (opts, _) = parse_options_with("test", &[], &[], &env).unwrap();
        assert_eq!(opts.log_dir, "C:\\logs");
        assert_eq!(opts.audit_dir, "C:\\audit");
        assert!(!opts.keep_awake);

        let env = env_of(&[("HANDOFF_CONFIG", &cfg), ("HANDOFF_LOG_DIR", "D:\\env")]);
        let (opts, _) = parse_options_with("test", &[], &[], &env).unwrap();
        assert_eq!(opts.log_dir, "D:\\env");
    }

    #[test]
    fn keep_awake_defaults_to_true() {
        let env = env_of(&[]);
        let (opts, _) = parse_options_with("test", &[], &[], &env).unwrap();
        assert!(opts.keep_awake);
    }

    #[test]
    fn flags_are_accepted_after_positional_arguments() {
        let extra = [Def::boolean("list", ""), Def::list("arg", "")];
        let env = env_of(&[]);
        let (opts, parsed) = parse_options_with(
            "exec",
            &strs(&[
                "n1_token",
                "app.list",
                "--arg",
                "limit=3",
                "--list",
                "--relay",
                "https://flag.example",
            ]),
            &extra,
            &env,
        )
        .unwrap();
        assert!(parsed.boolean("list"));
        assert_eq!(parsed.list("arg"), vec!["limit=3"]);
        assert_eq!(opts.relay, "https://flag.example");
        assert_eq!(parsed.args, vec!["n1_token", "app.list"]);
    }

    #[test]
    fn double_dash_stops_flag_parsing() {
        let env = env_of(&[]);
        let (_, parsed) =
            parse_options_with("exec", &strs(&["tok", "--", "--not-a-flag"]), &[], &env).unwrap();
        assert_eq!(parsed.args.len(), 2);
        assert_eq!(parsed.args[1], "--not-a-flag");
    }

    #[test]
    fn unknown_and_missing_value_flags_report_go_messages() {
        let env = env_of(&[]);
        let err = parse_options_with("t", &strs(&["--foo"]), &[], &env).unwrap_err();
        assert_eq!(
            err,
            FlagError::Message("flag provided but not defined: -foo".to_string())
        );
        let err = parse_options_with("t", &strs(&["--relay"]), &[], &env).unwrap_err();
        assert_eq!(
            err,
            FlagError::Message("flag needs an argument: -relay".to_string())
        );
        let err = parse_options_with("t", &strs(&["--json=x"]), &[], &env).unwrap_err();
        assert_eq!(
            err,
            FlagError::Message("invalid boolean value \"x\" for -json: parse error".to_string())
        );
    }

    #[test]
    fn help_flags_report_help() {
        let env = env_of(&[]);
        for flag in ["-h", "--help", "-help"] {
            assert_eq!(
                parse_options_with("t", &strs(&[flag]), &[], &env).unwrap_err(),
                FlagError::Help
            );
        }
    }

    #[test]
    fn a_flag_value_may_start_with_a_dash() {
        let extra = [Def::string("args-json", "", "")];
        let env = env_of(&[]);
        let (_, parsed) =
            parse_options_with("t", &strs(&["--args-json", "-1", "x"]), &extra, &env).unwrap();
        assert_eq!(parsed.string("args-json"), "-1");
        assert_eq!(parsed.args, vec!["x"]);
    }

    #[test]
    fn lone_dash_is_positional() {
        let env = env_of(&[]);
        let (_, parsed) = parse_options_with("t", &strs(&["-"]), &[], &env).unwrap();
        assert_eq!(parsed.args, vec!["-"]);
    }

    #[test]
    fn single_and_double_dash_are_equivalent_and_equals_works() {
        let env = env_of(&[]);
        let (opts, _) = parse_options_with(
            "t",
            &strs(&["-json", "--quiet=true", "-relay=https://a.example"]),
            &[],
            &env,
        )
        .unwrap();
        assert!(opts.json && opts.quiet);
        assert_eq!(opts.relay, "https://a.example");
    }

    #[test]
    fn usage_text_matches_the_go_flag_package_layout() {
        let extra = [
            Def::string("consent", "ask or deny", "ask"),
            Def::duration("timeout", "how long to wait", "1m0s"),
            Def::boolean("check", "report only").alias_of("check"),
            Def::boolean("c", "report only").alias_of("check"),
        ];
        let set = FlagSet::new("demo", &extra);
        let text = set.usage_text();
        assert!(text.starts_with("Usage of demo:\n"));
        assert!(text.contains("  -audit-dir string\n    \tdirectory for the audit log\n"));
        assert!(text.contains("  -c\treport only\n"));
        assert!(text.contains("  -consent string\n    \task or deny (default \"ask\")\n"));
        assert!(text.contains("  -json\n    \tprint machine-readable output\n"));
        assert!(text.contains("  -timeout duration\n    \thow long to wait (default 1m0s)\n"));
        let names: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("  -"))
            .map(|l| l.split(['\t', ' ']).nth(2).unwrap_or(""))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn durations_parse_from_flags() {
        let extra = [Def::duration("timeout", "", "1m0s")];
        let env = env_of(&[]);
        let (_, parsed) =
            parse_options_with("t", &strs(&["--timeout", "90s"]), &extra, &env).unwrap();
        assert_eq!(parsed.duration("timeout"), Duration::from_secs(90));
        let (_, parsed) = parse_options_with("t", &[], &extra, &env).unwrap();
        assert_eq!(parsed.duration("timeout"), Duration::from_secs(60));
        let err = parse_options_with("t", &strs(&["--timeout", "soon"]), &extra, &env).unwrap_err();
        assert_eq!(
            err,
            FlagError::Message("invalid value \"soon\" for flag -timeout: parse error".to_string())
        );
    }
}

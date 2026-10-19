use crate::cmd;

pub type Command = Box<dyn Fn(&[String]) -> i32>;

pub struct Commands {
    pub new: Command,
    pub menu: Command,
}

impl Default for Commands {
    fn default() -> Commands {
        Commands {
            new: Box::new(cmd::new::new),
            menu: Box::new(cmd::menu::menu),
        }
    }
}

pub fn wants_support_log(args: &[String]) -> bool {
    let sub = args.first().map(String::as_str).unwrap_or("");
    matches!(sub, "" | "new" | "tunnel" | "menu")
}

pub fn usage_text() -> String {
    [
        "handoff: remote help for Windows PCs",
        "",
        "Running without a subcommand shows an interactive menu.",
        "",
        "Subcommands:",
        "  new                        start a host session, print the view URL",
        "  menu                       show the interactive launcher",
        "  connect <url-or-token>     open an operator viewer (browser)",
        "  exec <token> <kind>        run one command against a session, print the result",
        "  tunnel <connect-token>     forward a remote local port to this computer",
        "  update [--check]           fetch a newer handoff.exe from the relay",
        "  version                    print version and file locations",
        "",
        "Common flags:",
        "  --relay URL                relay base URL",
        "  --json                     machine-readable output",
        "  --config FILE              config file path",
        "  --log-dir DIR              where to write the support log",
        "",
        "Env:",
        "  HANDOFF_RELAY      override the default relay URL (https://handoff.whyknot.dev)",
        "  HANDOFF_LOG_DIR    override the support log directory",
        "  HANDOFF_CONFIG     override the config file path",
        "",
    ]
    .join("\n")
}

pub fn run(args: &[String], commands: &Commands) -> i32 {
    let Some(first) = args.first() else {
        return (commands.menu)(&[]);
    };
    let rest = &args[1..];

    match first.as_str() {
        "new" => (commands.new)(rest),
        "menu" => (commands.menu)(rest),
        "connect" => cmd::connect::connect(rest),
        "tunnel" => cmd::tunnel::tunnel(rest),
        "exec" => cmd::exec::exec(rest),
        "update" => cmd::update::update(rest),
        "version" | "-v" | "--version" => cmd::version::print_version(rest, crate::VERSION),
        "help" | "-h" | "--help" => {
            print!("{}", usage_text());
            0
        }
        other => {
            eprintln!("unknown subcommand: {other}\n");
            print!("{}", usage_text());
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn run_without_args_shows_menu() {
        let called = Rc::new(RefCell::new(None::<Vec<String>>));
        let seen = called.clone();
        let commands = Commands {
            new: Box::new(|_| 0),
            menu: Box::new(move |args| {
                *seen.borrow_mut() = Some(args.to_vec());
                0
            }),
        };
        assert_eq!(run(&[], &commands), 0);
        assert_eq!(called.borrow().as_deref(), Some(&[][..]));
    }

    #[test]
    fn run_new_explicit_still_calls_host_session() {
        let called = Rc::new(RefCell::new(false));
        let seen = called.clone();
        let commands = Commands {
            new: Box::new(move |_| {
                *seen.borrow_mut() = true;
                0
            }),
            menu: Box::new(|_| 0),
        };
        assert_eq!(run(&["new".to_string()], &commands), 0);
        assert!(*called.borrow(), "new command was not called");
    }

    #[test]
    fn run_unknown_command_returns_usage_error() {
        let commands = Commands {
            new: Box::new(|_| 0),
            menu: Box::new(|_| 0),
        };
        assert_eq!(run(&["nope".to_string()], &commands), 2);
    }

    #[test]
    fn new_passes_its_arguments_through() {
        let seen = Rc::new(RefCell::new(Vec::<String>::new()));
        let sink = seen.clone();
        let commands = Commands {
            new: Box::new(move |args| {
                *sink.borrow_mut() = args.to_vec();
                7
            }),
            menu: Box::new(|_| 0),
        };
        let args: Vec<String> = ["new", "--consent", "deny"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(run(&args, &commands), 7);
        assert_eq!(*seen.borrow(), vec!["--consent", "deny"]);
    }

    #[test]
    fn support_log_is_only_for_long_running_subcommands() {
        let w =
            |s: &[&str]| wants_support_log(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert!(w(&[]));
        assert!(w(&["new"]));
        assert!(w(&["tunnel"]));
        assert!(w(&["menu"]));
        assert!(!w(&["version"]));
        assert!(!w(&["exec"]));
        assert!(!w(&["update"]));
    }

    #[test]
    fn usage_lists_every_subcommand() {
        let text = usage_text();
        for sub in [
            "new ", "menu ", "connect ", "exec ", "tunnel ", "update ", "version ",
        ] {
            assert!(text.contains(&format!("  {sub}")), "{sub}");
        }
        assert!(text.ends_with("HANDOFF_CONFIG     override the config file path\n"));
    }
}

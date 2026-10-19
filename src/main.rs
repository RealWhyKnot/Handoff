use handoff::app::{run, wants_support_log, Commands};
use handoff::{slog, supportlog, util};

fn main() {
    util::install_crypto_provider();

    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        slog!("panic: {info}");
        default_hook(info);
    }));

    let mut log_open = false;
    if wants_support_log(&args) {
        match supportlog::init() {
            Ok(path) => {
                log_open = true;
                if args.first().map(String::as_str) != Some("new") {
                    eprintln!("log: {}", path.display());
                }
            }
            Err(err) => eprintln!("warning: support log unavailable: {err}"),
        }
    }

    let code = run(&args, &Commands::default());

    if log_open {
        supportlog::close();
    }
    if code != 0 {
        std::process::exit(code);
    }
}

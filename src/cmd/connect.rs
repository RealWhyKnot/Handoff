use std::process::Command;

pub const USAGE: &str = "usage: handoff connect <view-url-or-token>";

pub fn connect_url(arg: &str, relay: &str) -> String {
    if arg.starts_with("http://") || arg.starts_with("https://") {
        return arg.to_string();
    }
    format!("{}/v/{}", relay.trim_end_matches('/'), arg)
}

pub fn default_relay() -> String {
    let relay = crate::util::env_var("HANDOFF_RELAY");
    if relay.is_empty() {
        super::flags::DEFAULT_RELAY_URL.to_string()
    } else {
        relay
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .map(|_| ())
}

pub fn connect(args: &[String]) -> i32 {
    let Some(arg) = args.first() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let url = connect_url(arg, &default_relay());

    if let Err(err) = open_browser(&url) {
        eprintln!("could not open browser: {err}");
        eprintln!("open this URL manually: {url}");
        return 1;
    }
    println!("opened: {url}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_urls_pass_through() {
        assert_eq!(
            connect_url("https://h.example/v/n1_abc", "https://other"),
            "https://h.example/v/n1_abc"
        );
        assert_eq!(connect_url("http://h/v/x", "https://other"), "http://h/v/x");
    }

    #[test]
    fn bare_tokens_use_the_relay() {
        assert_eq!(
            connect_url("n1_abc", "https://relay.example/"),
            "https://relay.example/v/n1_abc"
        );
    }

    #[test]
    fn missing_argument_is_a_usage_error() {
        assert_eq!(connect(&[]), 2);
    }
}

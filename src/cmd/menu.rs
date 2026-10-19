use std::io::{BufRead, Write};

use super::new::new;
use super::pause::{pause_if_own_console, stdin_is_terminal};
use super::tunnel::tunnel;
use super::update::update;
use super::version::print_version;

pub fn menu(args: &[String]) -> i32 {
    if !stdin_is_terminal() {
        eprintln!("handoff needs a terminal for the menu; run 'handoff new' to host a session");
        return 2;
    }

    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    loop {
        print_menu();
        let choice = match read_choice(&mut reader) {
            Ok(Some(c)) => c,
            Ok(None) => return 0,
            Err(err) => {
                eprintln!("could not read input: {err}");
                return 0;
            }
        };
        match choice.as_str() {
            "1" | "h" | "host" => {
                drop(reader);
                return new(args);
            }
            "2" | "t" | "tunnel" | "connect" => {
                return run_tunnel_prompt(&mut reader, args);
            }
            "3" | "u" | "update" => {
                drop(reader);
                let code = update(&[]);
                pause_if_own_console();
                return code;
            }
            "4" | "v" | "version" => {
                drop(reader);
                let code = print_version(&[], crate::VERSION);
                pause_if_own_console();
                return code;
            }
            "5" | "q" | "quit" | "exit" => return 0,
            "" => {}
            _ => println!("not a valid choice; type a number from 1 to 5."),
        }
    }
}

pub fn menu_text() -> &'static str {
    "\nWhat would you like to do?\n  1) Host a session (let someone help me with this computer)\n  2) Connect to a tunnel (forward a port from someone else's computer)\n  3) Check for updates\n  4) Show version and file locations\n  5) Quit\nChoose [1-5]: "
}

fn print_menu() {
    print!("{}", menu_text());
    let _ = std::io::stdout().flush();
}

fn read_choice(reader: &mut dyn BufRead) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_lowercase()))
}

pub fn parse_port_choice(input: &str) -> Option<u16> {
    match input.trim().parse::<i64>() {
        Ok(p) if p > 0 && p <= 65535 => Some(p as u16),
        _ => None,
    }
}

fn run_tunnel_prompt(reader: &mut dyn BufRead, base_args: &[String]) -> i32 {
    println!();
    println!("Paste the connect token shown on the session page after the host");
    println!("approves the tunnel (looks like 'tk_AbCdEfGh').");
    print!("Connect token: ");
    let _ = std::io::stdout().flush();
    let mut token_line = String::new();
    if let Err(err) = reader.read_line(&mut token_line) {
        eprintln!("could not read token: {err}");
        return 0;
    }
    let token = token_line.trim().to_string();
    if token.is_empty() {
        println!("no token entered; aborting.");
        return 0;
    }

    let mut port: u16 = 47800;
    loop {
        print!("Local port to bind on this computer [47800]: ");
        let _ = std::io::stdout().flush();
        let mut port_line = String::new();
        match reader.read_line(&mut port_line) {
            Ok(0) | Err(_) => {
                eprintln!("could not read port: EOF");
                return 0;
            }
            Ok(_) => {}
        }
        let v = port_line.trim();
        if v.is_empty() {
            break;
        }
        match parse_port_choice(v) {
            Some(p) => {
                port = p;
                break;
            }
            None => println!("port must be a whole number between 1 and 65535; try again."),
        }
    }

    let mut args = vec![token, "--local-port".to_string(), port.to_string()];
    args.extend_from_slice(base_args);
    tunnel(&args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn menu_lists_the_five_choices() {
        let text = menu_text();
        for needle in [
            "1) Host a session",
            "2) Connect to a tunnel",
            "3) Check for updates",
            "4) Show version",
            "5) Quit",
            "Choose [1-5]: ",
        ] {
            assert!(text.contains(needle), "{needle}");
        }
    }

    #[test]
    fn choices_are_trimmed_and_lowercased() {
        let mut input = Cursor::new(b"  HoSt \n".to_vec());
        assert_eq!(read_choice(&mut input).unwrap(), Some("host".to_string()));
        assert_eq!(read_choice(&mut input).unwrap(), None);
    }

    #[test]
    fn ports_must_be_whole_numbers_in_range() {
        assert_eq!(parse_port_choice("47800"), Some(47800));
        assert_eq!(parse_port_choice(" 80 "), Some(80));
        assert_eq!(parse_port_choice("0"), None);
        assert_eq!(parse_port_choice("65536"), None);
        assert_eq!(parse_port_choice("abc"), None);
    }

    #[test]
    fn blank_token_aborts_the_tunnel_prompt() {
        let mut input = Cursor::new(b"\n".to_vec());
        assert_eq!(run_tunnel_prompt(&mut input, &[]), 0);
    }
}

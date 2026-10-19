use std::io::{BufRead, IsTerminal, Write};

pub fn stdin_is_terminal() -> bool {
    std::io::stdin().is_terminal()
}

#[cfg(windows)]
pub fn owns_console() -> bool {
    use windows::Win32::System::Console::GetConsoleProcessList;

    let mut pids = [0u32; 4];
    let n = unsafe { GetConsoleProcessList(&mut pids) };
    n == 1
}

#[cfg(not(windows))]
pub fn owns_console() -> bool {
    false
}

pub fn pause_if_own_console() {
    if !owns_console() || !stdin_is_terminal() {
        return;
    }
    print!("\npress Enter to close...");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
}

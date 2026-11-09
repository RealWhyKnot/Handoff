use super::preview_inspect::check_fits;
use super::preview_inspect::first_visible_line;
use super::preview_inspect::focused_id;
use super::preview_inspect::gui_info;
use super::preview_inspect::inspect;
use super::preview_inspect::keyboard_run;
use super::preview_inspect::press;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) use super::preview_capture::capture;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};

pub(crate) const WIFI_SCRIPT: &str = r#"$ErrorActionPreference = 'Continue'
Write-Output '== Wi-Fi adapter'
Get-NetAdapter -Physical | Where-Object { $_.MediaType -match '802.11' } |
    Format-Table Name, InterfaceDescription, Status, LinkSpeed -AutoSize
Write-Output '== Current connection'
netsh wlan show interfaces
Write-Output '== Signal and nearby networks'
netsh wlan show networks mode=bssid | Select-String 'SSID|Signal|Channel|Radio type'
Write-Output '== IP configuration'
Get-NetIPConfiguration | Where-Object { $_.NetAdapter.Status -eq 'Up' } |
    Format-List InterfaceAlias, IPv4Address, IPv4DefaultGateway, DNSServer
Write-Output '== Gateway'
$gw = (Get-NetRoute -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | Select-Object -First 1).NextHop
if ($gw) {
    Test-Connection -ComputerName $gw -Count 4 -ErrorAction SilentlyContinue |
        Select-Object Address, Latency, Status
} else {
    Write-Output 'No default gateway found.'
}
Write-Output '== DNS'
foreach ($name in 'www.microsoft.com', 'one.one.one.one') {
    try { Resolve-DnsName $name -ErrorAction Stop | Select-Object -First 1 Name, IPAddress }
    catch { Write-Output "DNS lookup failed for ${name}: $($_.Exception.Message)" }
}
Write-Output '== Recent WLAN events'
Get-WinEvent -LogName 'Microsoft-Windows-WLAN-AutoConfig/Operational' -MaxEvents 15 |
    Select-Object TimeCreated, Id, LevelDisplayName | Format-Table -AutoSize
Write-Output '== Driver'
Get-PnpDevice -Class Net -PresentOnly | Where-Object FriendlyName -match 'Wi-?Fi|Wireless' |
    Format-Table FriendlyName, Status -AutoSize
& ipconfig /all | Select-String 'Wireless|Physical Address|DHCP Enabled|Lease'"#;

pub(crate) const SHORT_SCRIPT: &str = "Get-NetAdapter | Format-Table Name, Status, LinkSpeed";

pub(super) fn request(
    kind: &str,
    category: super::Category,
    summary: String,
    ask: super::Ask,
    detail: Option<&str>,
) -> super::Request {
    super::Request {
        kind: kind.into(),
        category: Some(category),
        summary,
        ask,
        ..super::Request::default()
    }
    .showing(detail)
}

fn script_request(script: &str, ask: super::Ask) -> super::Request {
    request(
        "ps.exec",
        super::Category::Exec,
        crate::capabilities::ps::consent_summary(script),
        ask,
        Some(script),
    )
}

pub(super) fn thread_of(hwnd: HWND) -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    // SAFETY: the call only reads the handle, and a stale one gives 0.
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

fn show_and_withdraw(name: &str, req: super::Request) {
    let answer = Arc::new(std::sync::Mutex::new(None));
    let got = Arc::clone(&answer);
    let shot = capture(
        name,
        super::PROMPT_TITLE,
        move || {
            let opts = super::dialog::Options {
                steal_focus: false,
                ..super::dialog::Options::default()
            };
            *got.lock().unwrap() = Some(super::dialog::show(&req, &opts));
        },
        |hwnd| super::dialog::close(thread_of(hwnd)),
    )
    .unwrap();
    crate::errln!(
        "{name}: {}x{} flags={}",
        shot.width,
        shot.height,
        shot.flags
    );
    assert_eq!(
        answer.lock().unwrap().clone(),
        Some(Ok(super::Answer::No)),
        "{name}"
    );
}

#[test]
#[ignore = "renders the consent prompt on a hidden desktop; run by hand"]
fn prompt_for_each_kind_of_request() {
    use super::{Ask, Category};
    let cases = [
        ("after-long.png", script_request(WIFI_SCRIPT, Ask::Default)),
        ("after-long-30.png", script_request(WIFI_SCRIPT, Ask::Minutes(30))),
        ("after-short.png", script_request(SHORT_SCRIPT, Ask::Default)),
        ("after-short-once.png", script_request(SHORT_SCRIPT, Ask::Once)),
        (
            "after-session-no-command.png",
            request(
                "tunnel.open",
                Category::Network,
                "Opens a network tunnel so the helper can reach your router at 192.168.1.1, port 80. It stays open until you end the session or the helper closes it.".into(),
                Ask::Default,
                None,
            ),
        ),
        (
            "after-power.png",
            request(
                "sys.restart",
                Category::Power,
                "Restarts this computer.".into(),
                Ask::Minutes(60),
                None,
            ),
        ),
    ];
    for (name, req) in cases {
        show_and_withdraw(name, req);
    }
}

fn oversized_cases() -> Vec<(&'static str, super::Request)> {
    use super::{Ask, Category};
    let path = format!("C:\\{}Old project backups", "Folder\\".repeat(700));
    let host = format!("{}.lan", "printer-".repeat(31));
    let long_name = format!("Invoke-{} -Path C:\\Temp", "VeryLongCommandName".repeat(60));
    let lines: String = (1..=2000).map(|i| format!("Write-Output {i}\n")).collect();
    vec![
        (
            "one-line",
            script_request(&"x".repeat(10_000), Ask::Minutes(240)),
        ),
        (
            "path",
            request(
                "fs.delete",
                Category::Files,
                crate::capabilities::fs_write::delete_summary(&path, true),
                Ask::Default,
                crate::capabilities::fs_write::path_detail(&path),
            ),
        ),
        (
            "wide",
            script_request(
                &format!(
                    "{}\n{}",
                    "网络诊断".repeat(300),
                    "\u{1F680}\u{2705}".repeat(300)
                ),
                Ask::Default,
            ),
        ),
        ("lines", script_request(&lines, Ask::Once)),
        ("command-name", script_request(&long_name, Ask::Minutes(5))),
        (
            "controls",
            script_request(
                &"\u{1b}[31mred\u{1b}[0m\r\n\u{202E}x\u{200B}\t\ty\n".repeat(200),
                Ask::Default,
            ),
        ),
        (
            "host",
            request(
                "tunnel.open",
                Category::Network,
                format!(
                    "Opens a network tunnel so the helper can reach {}. It stays open until you end the session or the helper closes it.",
                    crate::capabilities::tunnel::tunnel_target_description(&host, 8080)
                ),
                Ask::Default,
                None,
            ),
        ),
        (
            "service",
            request(
                "svc.control",
                Category::Services,
                format!(
                    "Runs 'restart' against Windows service \"{}\". This can interrupt software or device functionality.",
                    "Service".repeat(36) + "Name"
                ),
                Ask::Default,
                None,
            ),
        ),
    ]
}

#[test]
#[ignore = "renders the consent prompt on a hidden desktop; run by hand"]
fn oversized_requests_keep_the_prompt_on_screen() {
    use super::dialog::Area;
    let real = super::dialog::work_area();
    let small = Area {
        left: real.left,
        top: real.top,
        width: 1366,
        height: 720,
    };
    let layouts = [
        ("real", real, None),
        ("1366x768", small, None),
        ("real-150", real, Some(144)),
        ("1366x768-150", small, Some(144)),
    ];
    for (case, req) in oversized_cases() {
        let parts = super::prompt_parts(&req);
        let detail: Vec<String> = parts
            .detail
            .iter()
            .flat_map(|d| [d.clone(), format!("{}\n\n{d}", parts.summary)])
            .collect();
        for (layout, area, dpi) in layouts {
            let name = format!("oversized-{case}-{layout}.png");
            let req = req.clone();
            let seen = Arc::new(std::sync::Mutex::new(None));
            let seen_in = Arc::clone(&seen);
            let answer = Arc::new(std::sync::Mutex::new(None));
            let got = Arc::clone(&answer);
            let shot = capture(
                &name,
                super::PROMPT_TITLE,
                move || {
                    let opts = super::dialog::Options {
                        steal_focus: false,
                        area: Some(area),
                        dpi,
                        ..super::dialog::Options::default()
                    };
                    *got.lock().unwrap() = Some(super::dialog::show(&req, &opts));
                },
                move |hwnd| {
                    *seen_in.lock().unwrap() = Some(inspect(hwnd));
                    super::dialog::close(thread_of(hwnd));
                },
            )
            .unwrap_or_else(|e| {
                panic!(
                    "{name}: {e}; the prompt returned {:?}",
                    answer.lock().unwrap()
                )
            });
            assert_eq!(
                answer.lock().unwrap().clone(),
                Some(Ok(super::Answer::No)),
                "{name}"
            );
            let seen = seen.lock().unwrap().take().unwrap();
            let w = &seen.window;
            let edit = seen.parts.iter().find(|p| p.id == super::dialog::ID_DETAIL);
            crate::errln!(
                "{name}: {}x{} at ({}, {}) to ({}, {}) in {}x{}; box {}",
                shot.width,
                shot.height,
                w.left - area.left,
                w.top - area.top,
                w.right - area.left,
                w.bottom - area.top,
                area.width,
                area.height,
                edit.map_or("none".to_string(), |p| format!(
                    "{} units, {} px tall",
                    p.text_units,
                    p.rect.bottom - p.rect.top
                ))
            );
            check_fits(&name, &seen, area, &detail);
        }
    }
}

#[test]
#[ignore = "renders the consent prompt on a hidden desktop; run by hand"]
fn keyboard_reaches_every_control_and_enter_never_approves() {
    use super::dialog::{ID_DETAIL, ID_GRANT, ID_NO, ID_ONCE};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_DOWN, VK_ESCAPE, VK_NEXT, VK_RETURN, VK_TAB,
    };

    let req = || script_request(WIFI_SCRIPT, super::Ask::Default);

    let (trail, open, answer) = keyboard_run("keys-tab.png", req(), |t| {
        let mut seen = vec![focused_id(t)];
        for _ in 0..4 {
            press(t, VK_TAB.0);
            seen.push(focused_id(t));
        }
        seen
    });
    crate::errln!("tab order: {trail:?}, still open {open}");
    assert!(open);
    assert_eq!(trail, vec![ID_NO, ID_DETAIL, ID_GRANT, ID_ONCE, ID_NO]);
    assert_eq!(answer, Some(Ok(super::Answer::No)));

    let (trail, open, answer) = keyboard_run("keys-enter-box.png", req(), |t| {
        press(t, VK_TAB.0);
        let at = focused_id(t);
        let before = first_visible_line(t);
        press(t, VK_NEXT.0);
        press(t, VK_DOWN.0);
        let after = first_visible_line(t);
        press(t, VK_RETURN.0);
        vec![at, before, after]
    });
    crate::errln!("enter in the box: {trail:?}, still open {open} -> {answer:?}");
    assert_eq!(trail[0], ID_DETAIL);
    assert!(
        trail[2] > trail[1],
        "page down did not scroll the box: {trail:?}"
    );
    assert_eq!(answer, Some(Ok(super::Answer::No)));

    let (_, open, answer) = keyboard_run("keys-enter.png", req(), |t| {
        press(t, VK_RETURN.0);
        vec![]
    });
    crate::errln!("enter at the start: still open {open} -> {answer:?}");
    assert!(!open);
    assert_eq!(answer, Some(Ok(super::Answer::No)));

    let (_, open, answer) = keyboard_run("keys-escape.png", req(), |t| {
        press(t, VK_TAB.0);
        press(t, VK_TAB.0);
        press(t, VK_ESCAPE.0);
        vec![]
    });
    crate::errln!("escape from the grant button: still open {open} -> {answer:?}");
    assert!(!open);
    assert_eq!(answer, Some(Ok(super::Answer::No)));

    let (_, open, answer) = keyboard_run("keys-close.png", req(), |t| {
        use windows::Win32::UI::WindowsAndMessaging::{
            GA_ROOT, GetAncestor, PostMessageW, WM_CLOSE,
        };
        // SAFETY: the call only reads the handle.
        let top = unsafe { GetAncestor(gui_info(t).hwndFocus, GA_ROOT) };
        // SAFETY: WM_CLOSE carries no pointers.
        let _ = unsafe { PostMessageW(Some(top), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        std::thread::sleep(Duration::from_millis(150));
        vec![]
    });
    crate::errln!("closing the window: still open {open} -> {answer:?}");
    assert!(!open);
    assert_eq!(answer, Some(Ok(super::Answer::No)));

    let (_, open, answer) = keyboard_run("keys-enter-grant.png", req(), |t| {
        press(t, VK_TAB.0);
        press(t, VK_TAB.0);
        press(t, VK_RETURN.0);
        vec![]
    });
    crate::errln!("enter on a focused grant button: still open {open} -> {answer:?}");
    assert!(!open);
    assert_eq!(answer, Some(Ok(super::Answer::ForGrant)));

    let once = script_request(SHORT_SCRIPT, super::Ask::Once);
    let (trail, _, _) = keyboard_run("keys-once.png", once, |t| {
        let mut seen = vec![focused_id(t)];
        for _ in 0..3 {
            press(t, VK_TAB.0);
            seen.push(focused_id(t));
        }
        seen
    });
    crate::errln!("tab order without a grant button: {trail:?}");
    assert_eq!(trail, vec![ID_NO, ID_DETAIL, ID_ONCE, ID_NO]);
}

#[test]
#[ignore = "renders the consent prompt on a hidden desktop; run by hand"]
fn prompt_says_no_by_itself_when_time_runs_out() {
    let answer = Arc::new(std::sync::Mutex::new(None));
    let got = Arc::clone(&answer);
    let started = Instant::now();
    let shot = capture(
        "after-countdown.png",
        super::PROMPT_TITLE,
        move || {
            let req = script_request(SHORT_SCRIPT, super::Ask::Default);
            let opts = super::dialog::Options {
                steal_focus: false,
                timeout: Duration::from_secs(4),
                ..super::dialog::Options::default()
            };
            *got.lock().unwrap() = Some(super::dialog::show(&req, &opts));
        },
        |_| {},
    )
    .unwrap();
    crate::errln!("after-countdown.png: {}x{}", shot.width, shot.height);
    let took = started.elapsed();
    assert_eq!(
        answer.lock().unwrap().clone(),
        Some(Err(super::ConsentError::TimedOut))
    );
    assert!(
        took >= Duration::from_secs(4) && took < Duration::from_secs(7),
        "{took:?}"
    );
}

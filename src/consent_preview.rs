use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, GetWindowRect, GetWindowTextW, IsWindowVisible,
};

#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_ppm: i32,
    y_ppm: i32,
    clr_used: u32,
    clr_important: u32,
}

#[link(name = "user32")]
extern "system" {
    fn CreateDesktopW(
        name: *const u16,
        device: *const u16,
        devmode: *const c_void,
        flags: u32,
        access: u32,
        sa: *const c_void,
    ) -> *mut c_void;
    fn SetThreadDesktop(desk: *mut c_void) -> i32;
    fn CloseDesktop(desk: *mut c_void) -> i32;
    fn GetDC(hwnd: *mut c_void) -> *mut c_void;
    fn ReleaseDC(hwnd: *mut c_void, dc: *mut c_void) -> i32;
    fn PrintWindow(hwnd: *mut c_void, dc: *mut c_void, flags: u32) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(dc: *mut c_void) -> *mut c_void;
    fn CreateCompatibleBitmap(dc: *mut c_void, w: i32, h: i32) -> *mut c_void;
    fn SelectObject(dc: *mut c_void, obj: *mut c_void) -> *mut c_void;
    fn DeleteObject(obj: *mut c_void) -> i32;
    fn DeleteDC(dc: *mut c_void) -> i32;
    fn GetDIBits(
        dc: *mut c_void,
        bmp: *mut c_void,
        start: u32,
        lines: u32,
        bits: *mut c_void,
        info: *mut BitmapInfoHeader,
        usage: u32,
    ) -> i32;
}

const GENERIC_ALL: u32 = 0x1000_0000;
const PW_RENDERFULLCONTENT: u32 = 2;

pub fn out_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("ux-preview")
        .join("dialog");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn find_window(thread: u32, title: &str) -> Option<HWND> {
    struct Search {
        title: String,
        found: Option<HWND>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
        let search = unsafe { &mut *(lp.0 as *mut Search) };
        let mut buf = [0u16; 256];
        let n = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
        let text = String::from_utf16_lossy(&buf[..n]);
        if unsafe { IsWindowVisible(hwnd) }.as_bool() && text == search.title {
            search.found = Some(hwnd);
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut search = Search {
        title: title.to_string(),
        found: None,
    };
    unsafe {
        let _ = EnumThreadWindows(
            thread,
            Some(each),
            LPARAM(&mut search as *mut Search as isize),
        );
    }
    search.found
}

fn grab(hwnd: HWND, flags: u32) -> Option<(u32, u32, Vec<u8>)> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    let raw = hwnd.0;
    unsafe {
        let screen = GetDC(raw);
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        let printed = PrintWindow(raw, mem, flags);
        let mut info = BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width: w,
            height: -h,
            planes: 1,
            bit_count: 32,
            compression: 0,
            size_image: 0,
            x_ppm: 0,
            y_ppm: 0,
            clr_used: 0,
            clr_important: 0,
        };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            bgra.as_mut_ptr() as *mut c_void,
            &mut info,
            0,
        );
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(raw, screen);
        if printed == 0 || lines != h {
            return None;
        }
        let mut rgba = Vec::with_capacity(bgra.len());
        for px in bgra.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        let first = &rgba[..4];
        if rgba.chunks_exact(4).all(|p| p == first) {
            return None;
        }
        Some((w as u32, h as u32, rgba))
    }
}

fn save_png(path: &PathBuf, w: u32, h: u32, rgba: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(rgba).unwrap();
}

pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub flags: u32,
}

pub fn capture<S, C>(name: &str, title: &str, show: S, close: C) -> Result<Shot, String>
where
    S: FnOnce() + Send + 'static,
    C: Fn(HWND) + Send + 'static,
{
    let desk_name = wide(&format!("handoff-preview-{}", std::process::id()));
    let desk = unsafe {
        CreateDesktopW(
            desk_name.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            GENERIC_ALL,
            std::ptr::null(),
        )
    };
    if desk.is_null() {
        return Err(format!(
            "CreateDesktopW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let desk_addr = desk as usize;
    let thread = Arc::new(AtomicU32::new(0));
    let seen = thread.clone();
    let shower = std::thread::spawn(move || {
        if unsafe { SetThreadDesktop(desk_addr as *mut c_void) } == 0 {
            return Err(format!(
                "SetThreadDesktop failed on the dialog thread: {}",
                std::io::Error::last_os_error()
            ));
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        seen.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        show();
        Ok(())
    });
    let path = out_dir().join(name);
    let title = title.to_string();
    let grabber = std::thread::spawn(move || -> Result<Shot, String> {
        if unsafe { SetThreadDesktop(desk_addr as *mut c_void) } == 0 {
            return Err(format!(
                "SetThreadDesktop failed on the capture thread: {}",
                std::io::Error::last_os_error()
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let hwnd = loop {
            let id = thread.load(Ordering::SeqCst);
            if id != 0 {
                if let Some(h) = find_window(id, &title) {
                    break h;
                }
            }
            if Instant::now() > deadline {
                return Err("the dialog window never appeared".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        std::thread::sleep(Duration::from_millis(1200));
        let mut result = Err("PrintWindow gave an empty image with both flag sets".to_string());
        for flags in [PW_RENDERFULLCONTENT, 0] {
            if let Some((w, h, rgba)) = grab(hwnd, flags) {
                save_png(&path, w, h, &rgba);
                result = Ok(Shot {
                    width: w,
                    height: h,
                    flags,
                });
                break;
            }
        }
        close(hwnd);
        result
    });
    let shot = grabber
        .join()
        .map_err(|_| "capture thread panicked".to_string())?;
    let shown = shower
        .join()
        .map_err(|_| "dialog thread panicked".to_string())?;
    unsafe { CloseDesktop(desk) };
    shown?;
    shot
}

pub const WIFI_SCRIPT: &str = r#"$ErrorActionPreference = 'Continue'
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

pub const SHORT_SCRIPT: &str = "Get-NetAdapter | Format-Table Name, Status, LinkSpeed";

fn request(
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

fn thread_of(hwnd: HWND) -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

fn show_and_withdraw(name: &str, req: super::Request) {
    let answer = Arc::new(std::sync::Mutex::new(None));
    let got = answer.clone();
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
    eprintln!(
        "{name}: {}x{} flags={}",
        shot.width, shot.height, shot.flags
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

struct Part {
    id: i32,
    class: String,
    rect: RECT,
    text_units: usize,
    needed: (i32, i32),
}

struct Seen {
    window: RECT,
    client: RECT,
    parts: Vec<Part>,
}

fn inspect(hwnd: HWND) -> Seen {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{ClientToScreen, HFONT};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, GetClassNameW, GetClientRect, GetDlgCtrlID, GetWindowTextLengthW,
        SendMessageW, WM_GETFONT,
    };

    unsafe extern "system" fn each(child: HWND, lp: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lp.0 as *mut Vec<HWND>) };
        out.push(child);
        BOOL(1)
    }
    let mut kids: Vec<HWND> = Vec::new();
    let mut window = RECT::default();
    let mut client = RECT::default();
    let mut origin = POINT::default();
    unsafe {
        let _ = EnumChildWindows(
            Some(hwnd),
            Some(each),
            LPARAM(&mut kids as *mut Vec<HWND> as isize),
        );
        let _ = GetWindowRect(hwnd, &mut window);
        let _ = GetClientRect(hwnd, &mut client);
        let _ = ClientToScreen(hwnd, &mut origin);
    }
    let mut parts = Vec::new();
    for kid in kids {
        let mut class = [0u16; 64];
        let n = unsafe { GetClassNameW(kid, &mut class) } as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        let mut r = RECT::default();
        let _ = unsafe { GetWindowRect(kid, &mut r) };
        let rect = RECT {
            left: r.left - origin.x,
            top: r.top - origin.y,
            right: r.right - origin.x,
            bottom: r.bottom - origin.y,
        };
        let units = unsafe { GetWindowTextLengthW(kid) }.max(0) as usize;
        let mut buf = vec![0u16; units + 1];
        let got = unsafe { GetWindowTextW(kid, &mut buf) } as usize;
        let text = String::from_utf16_lossy(&buf[..got]);
        let font = HFONT(unsafe { SendMessageW(kid, WM_GETFONT, None, None) }.0 as *mut _);
        let width = rect.right - rect.left;
        let needed = if class.eq_ignore_ascii_case("Button") {
            super::dialog::measure(font, &text, 10_000, true)
        } else if class.eq_ignore_ascii_case("Static") {
            let one = super::dialog::measure(font, &text, 10_000, true);
            let wrapped = super::dialog::measure(font, &text, width, false);
            if one.0 <= width {
                one
            } else {
                wrapped
            }
        } else {
            (0, 0)
        };
        parts.push(Part {
            id: unsafe { GetDlgCtrlID(kid) },
            class,
            rect,
            text_units: units,
            needed,
        });
    }
    Seen {
        window,
        client,
        parts,
    }
}

fn check_fits(name: &str, seen: &Seen, area: super::dialog::Area, detail: &[String]) {
    let w = &seen.window;
    assert!(
        w.left >= area.left
            && w.top >= area.top
            && w.right <= area.left + area.width
            && w.bottom <= area.top + area.height,
        "{name}: window ({}, {}) to ({}, {}) leaves the {}x{} work area",
        w.left,
        w.top,
        w.right,
        w.bottom,
        area.width,
        area.height
    );
    for p in &seen.parts {
        let r = &p.rect;
        assert!(
            r.left >= 0
                && r.top >= 0
                && r.right <= seen.client.right
                && r.bottom <= seen.client.bottom,
            "{name}: {} {} at ({}, {}) to ({}, {}) is outside the {}x{} client area",
            p.class,
            p.id,
            r.left,
            r.top,
            r.right,
            r.bottom,
            seen.client.right,
            seen.client.bottom
        );
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if p.class.eq_ignore_ascii_case("Button") {
            assert!(
                p.needed.0 + 8 <= w && p.needed.1 <= h,
                "{name}: button {} needs {:?} but has {w}x{h}",
                p.id,
                p.needed
            );
        }
        if p.class.eq_ignore_ascii_case("Static") {
            assert!(
                p.needed.0 <= w && p.needed.1 <= h,
                "{name}: text needs {:?} but its box is {w}x{h}",
                p.needed
            );
        }
    }
    let ids: Vec<i32> = seen.parts.iter().map(|p| p.id).collect();
    for id in [super::dialog::ID_ONCE, super::dialog::ID_NO] {
        assert!(ids.contains(&id), "{name}: button {id} is missing");
    }
    if !detail.is_empty() {
        let edit = seen
            .parts
            .iter()
            .find(|p| p.id == super::dialog::ID_DETAIL)
            .unwrap_or_else(|| panic!("{name}: no text box"));
        let want: Vec<usize> = detail
            .iter()
            .map(|d| super::dialog::crlf(d).encode_utf16().count())
            .collect();
        assert!(
            want.contains(&edit.text_units),
            "{name}: the box holds {} UTF-16 units, the request has {want:?}",
            edit.text_units
        );
    }
}

fn oversized_cases() -> Vec<(&'static str, super::Request)> {
    use super::{Ask, Category};
    let path = format!("C:\\{}Old project backups", "Folder\\".repeat(700));
    let host = format!("{}.lan", "printer-".repeat(31));
    let long_name = format!("Invoke-{} -Path C:\\Temp", "VeryLongCommandName".repeat(60));
    let lines: String = (1..=2000).map(|i| format!("Write-Output {i}\n")).collect();
    vec![
        ("one-line", script_request(&"x".repeat(10_000), Ask::Minutes(240))),
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
            let seen_in = seen.clone();
            let answer = Arc::new(std::sync::Mutex::new(None));
            let got = answer.clone();
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
            eprintln!(
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

fn focused_id(thread: u32) -> i32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetDlgCtrlID, GetGUIThreadInfo, GUITHREADINFO};
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetGUIThreadInfo(thread, &mut info) }.is_err() || info.hwndFocus.is_invalid() {
        return 0;
    }
    unsafe { GetDlgCtrlID(info.hwndFocus) }
}

fn first_visible_line(thread: u32) -> i32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetGUIThreadInfo, SendMessageW, GUITHREADINFO};
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let _ = unsafe { GetGUIThreadInfo(thread, &mut info) };
    unsafe { SendMessageW(info.hwndFocus, 0xCE, None, None) }.0 as i32
}

fn press(thread: u32, key: u16) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetGUIThreadInfo, PostMessageW, GUITHREADINFO, WM_KEYDOWN, WM_KEYUP,
    };
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let _ = unsafe { GetGUIThreadInfo(thread, &mut info) };
    let target = Some(info.hwndFocus);
    unsafe {
        let _ = PostMessageW(target, WM_KEYDOWN, WPARAM(key as usize), LPARAM(1));
        let _ = PostMessageW(
            target,
            WM_KEYUP,
            WPARAM(key as usize),
            LPARAM(1 | (1 << 30) | (1 << 31)),
        );
    }
    std::thread::sleep(Duration::from_millis(150));
}

fn keyboard_run(
    name: &str,
    req: super::Request,
    keys: impl Fn(u32) -> Vec<i32> + Send + 'static,
) -> (
    Vec<i32>,
    bool,
    Option<Result<super::Answer, super::ConsentError>>,
) {
    let answer = Arc::new(std::sync::Mutex::new(None));
    let got = answer.clone();
    let trail = Arc::new(std::sync::Mutex::new(Vec::new()));
    let trail_in = trail.clone();
    let open = Arc::new(std::sync::Mutex::new(false));
    let open_in = open.clone();
    capture(
        name,
        super::PROMPT_TITLE,
        move || {
            let opts = super::dialog::Options {
                steal_focus: false,
                ..super::dialog::Options::default()
            };
            *got.lock().unwrap() = Some(super::dialog::show(&req, &opts));
        },
        move |hwnd| {
            use windows::Win32::UI::WindowsAndMessaging::IsWindow;
            let thread = thread_of(hwnd);
            *trail_in.lock().unwrap() = keys(thread);
            if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                *open_in.lock().unwrap() = true;
                super::dialog::close(thread);
            }
        },
    )
    .unwrap();
    let trail = trail.lock().unwrap().clone();
    let answer = answer.lock().unwrap().clone();
    let open = *open.lock().unwrap();
    (trail, open, answer)
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
    eprintln!("tab order: {trail:?}, still open {open}");
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
    eprintln!("enter in the box: {trail:?}, still open {open} -> {answer:?}");
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
    eprintln!("enter at the start: still open {open} -> {answer:?}");
    assert!(!open);
    assert_eq!(answer, Some(Ok(super::Answer::No)));

    let (_, open, answer) = keyboard_run("keys-escape.png", req(), |t| {
        press(t, VK_TAB.0);
        press(t, VK_TAB.0);
        press(t, VK_ESCAPE.0);
        vec![]
    });
    eprintln!("escape from the grant button: still open {open} -> {answer:?}");
    assert!(!open);
    assert_eq!(answer, Some(Ok(super::Answer::No)));

    let (_, open, answer) = keyboard_run("keys-close.png", req(), |t| {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetAncestor, GetGUIThreadInfo, PostMessageW, GA_ROOT, GUITHREADINFO, WM_CLOSE,
        };
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let _ = unsafe { GetGUIThreadInfo(t, &mut info) };
        let top = unsafe { GetAncestor(info.hwndFocus, GA_ROOT) };
        let _ = unsafe { PostMessageW(Some(top), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        std::thread::sleep(Duration::from_millis(150));
        vec![]
    });
    eprintln!("closing the window: still open {open} -> {answer:?}");
    assert!(!open);
    assert_eq!(answer, Some(Ok(super::Answer::No)));

    let (_, open, answer) = keyboard_run("keys-enter-grant.png", req(), |t| {
        press(t, VK_TAB.0);
        press(t, VK_TAB.0);
        press(t, VK_RETURN.0);
        vec![]
    });
    eprintln!("enter on a focused grant button: still open {open} -> {answer:?}");
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
    eprintln!("tab order without a grant button: {trail:?}");
    assert_eq!(trail, vec![ID_NO, ID_DETAIL, ID_ONCE, ID_NO]);
}

#[test]
#[ignore = "renders the consent prompt on a hidden desktop; run by hand"]
fn prompt_says_no_by_itself_when_time_runs_out() {
    let answer = Arc::new(std::sync::Mutex::new(None));
    let got = answer.clone();
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
    eprintln!("after-countdown.png: {}x{}", shot.width, shot.height);
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

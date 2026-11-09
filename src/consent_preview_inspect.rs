use super::preview::thread_of;
use super::preview_capture::capture;
use std::sync::Arc;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, GetWindowTextW};
use windows::core::BOOL;

pub(super) struct Part {
    pub(super) id: i32,
    pub(super) class: String,
    pub(super) rect: RECT,
    pub(super) text_units: usize,
    pub(super) needed: (i32, i32),
}

pub(super) struct Seen {
    pub(super) window: RECT,
    pub(super) client: RECT,
    pub(super) parts: Vec<Part>,
}

pub(super) fn inspect(hwnd: HWND) -> Seen {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{ClientToScreen, HFONT};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, GetClassNameW, GetClientRect, GetDlgCtrlID, GetWindowTextLengthW,
        SendMessageW, WM_GETFONT,
    };

    unsafe extern "system" fn each(child: HWND, lp: LPARAM) -> BOOL {
        let ptr = std::ptr::with_exposed_provenance_mut::<Vec<HWND>>(lp.0.cast_unsigned());
        // SAFETY: inspect passes a Vec that outlives the enumeration.
        let out = unsafe { &mut *ptr };
        out.push(child);
        BOOL(1)
    }
    let mut kids: Vec<HWND> = Vec::new();
    let mut window = RECT::default();
    let mut client = RECT::default();
    let mut origin = POINT::default();
    let target = std::ptr::from_mut(&mut kids)
        .expose_provenance()
        .cast_signed();
    // SAFETY: kids and the three out values stay alive for the length of the calls.
    unsafe {
        let _ = EnumChildWindows(Some(hwnd), Some(each), LPARAM(target));
        let _ = GetWindowRect(hwnd, &mut window);
        let _ = GetClientRect(hwnd, &mut client);
        let _ = ClientToScreen(hwnd, &mut origin);
    }
    let mut parts = Vec::new();
    for kid in kids {
        let mut class = [0u16; 64];
        // SAFETY: kid came from EnumChildWindows and class is a live buffer.
        let n = usize::try_from(unsafe { GetClassNameW(kid, &mut class) }).unwrap_or(0);
        let class = String::from_utf16_lossy(&class[..n]);
        let mut r = RECT::default();
        // SAFETY: r is a live RECT the call fills in.
        let _ = unsafe { GetWindowRect(kid, &mut r) };
        let rect = RECT {
            left: r.left - origin.x,
            top: r.top - origin.y,
            right: r.right - origin.x,
            bottom: r.bottom - origin.y,
        };
        // SAFETY: the call only reads the handle.
        let units = usize::try_from(unsafe { GetWindowTextLengthW(kid) }).unwrap_or(0);
        let mut buf = vec![0u16; units + 1];
        // SAFETY: buf is a live buffer one unit longer than the text.
        let got = usize::try_from(unsafe { GetWindowTextW(kid, &mut buf) }).unwrap_or(0);
        let text = String::from_utf16_lossy(&buf[..got]);
        // SAFETY: WM_GETFONT takes no pointers.
        let handle = unsafe { SendMessageW(kid, WM_GETFONT, None, None) }.0;
        let font = HFONT(std::ptr::with_exposed_provenance_mut(
            handle.cast_unsigned(),
        ));
        let width = rect.right - rect.left;
        let needed = if class.eq_ignore_ascii_case("Button") {
            super::dialog::measure(font, &text, 10_000, true)
        } else if class.eq_ignore_ascii_case("Static") {
            let one = super::dialog::measure(font, &text, 10_000, true);
            let wrapped = super::dialog::measure(font, &text, width, false);
            if one.0 <= width { one } else { wrapped }
        } else {
            (0, 0)
        };
        // SAFETY: the call only reads the handle.
        let id = unsafe { GetDlgCtrlID(kid) };
        parts.push(Part {
            id,
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

pub(super) fn check_fits(name: &str, seen: &Seen, area: super::dialog::Area, detail: &[String]) {
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

pub(super) fn gui_info(thread: u32) -> windows::Win32::UI::WindowsAndMessaging::GUITHREADINFO {
    use windows::Win32::UI::WindowsAndMessaging::{GUITHREADINFO, GetGUIThreadInfo};
    let mut info = GUITHREADINFO {
        cbSize: u32::try_from(std::mem::size_of::<GUITHREADINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: info is a live GUITHREADINFO with cbSize set.
    let _ = unsafe { GetGUIThreadInfo(thread, &mut info) };
    info
}

pub(super) fn focused_id(thread: u32) -> i32 {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgCtrlID;
    let info = gui_info(thread);
    if info.hwndFocus.is_invalid() {
        return 0;
    }
    // SAFETY: the call only reads the handle.
    unsafe { GetDlgCtrlID(info.hwndFocus) }
}

pub(super) fn first_visible_line(thread: u32) -> i32 {
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    let info = gui_info(thread);
    // SAFETY: EM_GETFIRSTVISIBLELINE takes no pointers.
    let line = unsafe { SendMessageW(info.hwndFocus, 0xCE, None, None) }.0;
    i32::try_from(line).unwrap_or(0)
}

pub(super) fn press(thread: u32, key: u16) {
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP};
    let target = Some(gui_info(thread).hwndFocus);
    // SAFETY: key messages carry no pointers.
    unsafe {
        let _ = PostMessageW(target, WM_KEYDOWN, WPARAM(usize::from(key)), LPARAM(1));
        let _ = PostMessageW(
            target,
            WM_KEYUP,
            WPARAM(usize::from(key)),
            LPARAM(1 | (1 << 30) | (1 << 31)),
        );
    }
    std::thread::sleep(Duration::from_millis(150));
}

pub(super) fn keyboard_run(
    name: &str,
    req: super::Request,
    keys: impl Fn(u32) -> Vec<i32> + Send + 'static,
) -> (
    Vec<i32>,
    bool,
    Option<Result<super::Answer, super::ConsentError>>,
) {
    let answer = Arc::new(std::sync::Mutex::new(None));
    let got = Arc::clone(&answer);
    let trail = Arc::new(std::sync::Mutex::new(Vec::new()));
    let trail_in = Arc::clone(&trail);
    let open = Arc::new(std::sync::Mutex::new(false));
    let open_in = Arc::clone(&open);
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
            // SAFETY: the call only reads the handle.
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

use super::dialog_layout::Fonts;
use super::dialog_layout::STYLE;
use super::dialog_layout::layout;
use super::dialog_layout::rect;
use super::dialog_proc::State;
use super::dialog_proc::proc_;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DestroyWindow, DispatchMessageW, EnumThreadWindows,
    GetMessageW, IDC_ARROW, IDNO, IDYES, IsDialogMessageW, LoadCursorW, MB_ICONWARNING,
    MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MSG, MessageBoxW, PostMessageW, RegisterClassExW,
    SetForegroundWindow, TranslateMessage, WM_COMMAND, WM_USER, WNDCLASSEXW, WS_EX_DLGMODALFRAME,
    WS_EX_TOPMOST, WS_VISIBLE,
};
use windows::core::{BOOL, HSTRING, w};

pub use super::dialog_layout::Area;
pub use super::dialog_layout::crlf;
pub use super::dialog_layout::measure;
pub use super::dialog_layout::work_area;
use super::{Answer, ConsentError, PROMPT_TIMEOUT, PROMPT_TITLE, Request, Term, prompt_text};

pub const ID_GRANT: i32 = 1001;
pub const ID_ONCE: i32 = 1002;
pub const ID_NO: i32 = 1003;
pub const ID_DETAIL: i32 = 1020;

pub const BOX_ROWS_MAX: i32 = 14;
pub const BOX_ROWS_MIN: i32 = 3;

pub(super) const SS_NOPREFIX: u32 = 0x80;
pub(super) const SS_EDITCONTROL: u32 = 0x2000;
pub(super) const ES_MULTILINE: u32 = 0x4;
pub(super) const ES_READONLY: u32 = 0x800;
pub(super) const BS_DEFPUSHBUTTON: u32 = 0x1;
pub(super) const EM_SETLIMITTEXT: u32 = 0xC5;
pub(super) const DM_GETDEFID: u32 = WM_USER;
pub(super) const DC_HASDEFID: isize = 0x534B;
pub(super) const TIMER: usize = 7;
pub(super) const EDIT_SUBCLASS: usize = 1;

pub(super) const WHITE: COLORREF = COLORREF(0x00FF_FFFF);
pub(super) const BAND: COLORREF = COLORREF(0x00F0_F0F0);
const LINE: COLORREF = COLORREF(0x00DF_DFDF);
pub(super) const BLUE: COLORREF = COLORREF(0x0099_3300);

pub struct Options {
    pub steal_focus: bool,
    pub timeout: Duration,
    pub area: Option<Area>,
    pub dpi: Option<i32>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            steal_focus: true,
            timeout: PROMPT_TIMEOUT,
            area: None,
            dpi: None,
        }
    }
}

pub fn show(req: &Request, opts: &Options) -> Result<Answer, ConsentError> {
    let f = Fonts::new(opts.dpi);
    let area = opts.area.unwrap_or_else(work_area);
    let lay = layout(&f, req, area, opts.timeout);
    // SAFETY: CreateSolidBrush takes a colour only; State's Drop deletes the brushes.
    let (white, band, line) = unsafe {
        (
            CreateSolidBrush(WHITE),
            CreateSolidBrush(BAND),
            CreateSolidBrush(LINE),
        )
    };
    // SAFETY: LoadCursorW with no module loads a shared system cursor.
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default();
    let st = Box::into_raw(Box::new(State {
        children: lay.children,
        statics: Vec::new(),
        band_top: lay.band_top,
        footer_top: lay.footer_top,
        width: lay.width,
        height: lay.height,
        icon_at: (f.px(14), f.px(14), f.px(32)),
        timeout: opts.timeout,
        started: Instant::now(),
        shown_secs: opts.timeout.as_secs(),
        footer: None,
        result: None,
        timed_out: false,
        done: false,
        white,
        band,
        line,
    }));

    let class = w!("HandoffConsentPrompt");
    let wc = WNDCLASSEXW {
        cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or(0),
        lpfnWndProc: Some(proc_),
        hCursor: cursor,
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: wc is a live WNDCLASSEXW whose class name is a static string.
    unsafe { RegisterClassExW(&wc) };
    let ex = if opts.steal_focus {
        WS_EX_DLGMODALFRAME | WS_EX_TOPMOST
    } else {
        WS_EX_DLGMODALFRAME
    };
    let mut r = rect(0, 0, lay.width, lay.height);
    // SAFETY: r is a live RECT the call adjusts in place.
    let _ = unsafe { AdjustWindowRectEx(&mut r, STYLE, false, ex) };
    let (ow, oh) = (r.right - r.left, r.bottom - r.top);
    let x = area.left + (area.width - ow).max(0) / 2;
    let y = area.top + (area.height - oh).max(0) / 2;
    let title = HSTRING::from(PROMPT_TITLE);
    // SAFETY: class and title outlive the call, and st stays allocated until the loop below ends.
    let created = unsafe {
        CreateWindowExW(
            ex,
            class,
            &title,
            STYLE | WS_VISIBLE,
            x,
            y,
            ow,
            oh,
            None,
            None,
            None,
            Some(st.cast_const().cast()),
        )
    };
    let hwnd = match created {
        Ok(h) => h,
        Err(_) => {
            // SAFETY: st came from Box::into_raw above and no window holds it.
            drop(unsafe { Box::from_raw(st) });
            return message_box(req, opts);
        }
    };
    if opts.steal_focus {
        // SAFETY: hwnd is the window created above.
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
    }
    let mut msg = MSG::default();
    loop {
        // SAFETY: st is still allocated, and the window procedure only runs on this thread.
        if unsafe { (*st).done } {
            break;
        }
        // SAFETY: msg is a live MSG the call fills in.
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if got.0 <= 0 {
            break;
        }
        // SAFETY: msg was just filled in by GetMessageW and hwnd is the dialog window.
        unsafe {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    // SAFETY: st is still allocated, and the window procedure only runs on this thread.
    if !unsafe { (*st).done } {
        // SAFETY: st is still allocated and hwnd is still open, since done was false.
        unsafe {
            (*st).done = true;
            let _ = DestroyWindow(hwnd);
        }
    }
    // SAFETY: st came from Box::into_raw above and the window that used it is destroyed.
    let st = unsafe { Box::from_raw(st) };
    if st.timed_out {
        return Err(ConsentError::TimedOut);
    }
    Ok(match st.result {
        Some(ID_GRANT) => Answer::ForGrant,
        Some(ID_ONCE) => Answer::Once,
        _ => Answer::No,
    })
}

fn message_box(req: &Request, opts: &Options) -> Result<Answer, ConsentError> {
    let text = HSTRING::from(prompt_text(req));
    let title = HSTRING::from(PROMPT_TITLE);
    let mut style = MB_YESNO | MB_ICONWARNING;
    if opts.steal_focus {
        style |= MB_SETFOREGROUND | MB_TOPMOST;
    }
    // SAFETY: text and title are live strings for the length of the call.
    let ret = unsafe { MessageBoxW(None, &text, &title, style) };
    if ret.0 == 0 {
        return Err(ConsentError::Other(format!(
            "consent prompt failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(match (ret == IDYES, req.term()) {
        (false, _) => Answer::No,
        (true, Term::Once) => Answer::Once,
        (true, _) => Answer::ForGrant,
    })
}

pub fn close(thread_id: u32) {
    unsafe extern "system" fn press_no(hwnd: HWND, _: LPARAM) -> BOOL {
        let no = WPARAM(usize::try_from(IDNO.0).unwrap_or(0));
        // SAFETY: hwnd is a live window EnumThreadWindows handed to this callback.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_COMMAND, no, LPARAM(0)) };
        BOOL(1)
    }
    // SAFETY: press_no matches the callback signature and uses no state.
    unsafe {
        let _ = EnumThreadWindows(thread_id, Some(press_no), LPARAM(0));
    }
}

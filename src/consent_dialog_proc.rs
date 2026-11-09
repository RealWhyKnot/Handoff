use super::countdown_text;
use super::dialog::BAND;
use super::dialog::BLUE;
use super::dialog::DC_HASDEFID;
use super::dialog::DM_GETDEFID;
use super::dialog::EDIT_SUBCLASS;
use super::dialog::EM_SETLIMITTEXT;
use super::dialog::ID_DETAIL;
use super::dialog::ID_GRANT;
use super::dialog::ID_NO;
use super::dialog::ID_ONCE;
use super::dialog::SS_EDITCONTROL;
use super::dialog::SS_NOPREFIX;
use super::dialog::TIMER;
use super::dialog::WHITE;
use super::dialog_layout::crlf;
use super::dialog_layout::rect;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, DeleteObject, EndPaint, FillRect, HBRUSH, HDC, HFONT, HGDIOBJ, PAINTSTRUCT,
    SetBkColor, SetTextColor,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DI_NORMAL, DLGC_HASSETSEL, DLGC_WANTALLKEYS, DLGC_WANTTAB,
    DefWindowProcW, DestroyWindow, DrawIconEx, GWLP_USERDATA, GetWindowLongPtrW, HMENU, IDCANCEL,
    IDI_WARNING, IDNO, KillTimer, LoadIconW, SendMessageW, SetTimer, SetWindowLongPtrW,
    SetWindowTextW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CREATE,
    WM_CTLCOLORSTATIC, WM_GETDLGCODE, WM_NCDESTROY, WM_PAINT, WM_SETFONT, WM_TIMER, WS_CHILD,
    WS_VISIBLE,
};
use windows::core::{HSTRING, PCWSTR, w};

#[derive(Clone, Copy)]
pub(super) enum Role {
    Plain,
    Big,
    Footer,
}

pub(super) struct Child {
    pub(super) class: PCWSTR,
    pub(super) text: String,
    pub(super) style: u32,
    pub(super) ex: u32,
    pub(super) id: i32,
    pub(super) rect: RECT,
    pub(super) font: HFONT,
    pub(super) role: Role,
}

pub(super) fn label(text: &str, rect: RECT, font: HFONT, role: Role) -> Child {
    Child {
        class: w!("STATIC"),
        text: text.to_string(),
        style: SS_NOPREFIX | SS_EDITCONTROL,
        ex: 0,
        id: 0,
        rect,
        font,
        role,
    }
}

pub(super) struct State {
    pub(super) children: Vec<Child>,
    pub(super) statics: Vec<(HWND, Role)>,
    pub(super) band_top: i32,
    pub(super) footer_top: i32,
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) icon_at: (i32, i32, i32),
    pub(super) timeout: Duration,
    pub(super) started: Instant,
    pub(super) shown_secs: u64,
    pub(super) footer: Option<HWND>,
    pub(super) result: Option<i32>,
    pub(super) timed_out: bool,
    pub(super) done: bool,
    pub(super) white: HBRUSH,
    pub(super) band: HBRUSH,
    pub(super) line: HBRUSH,
}

impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: the three brushes were made in show and are deleted only here.
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.white.0));
            let _ = DeleteObject(HGDIOBJ(self.band.0));
            let _ = DeleteObject(HGDIOBJ(self.line.0));
        }
    }
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_NCDESTROY {
        // SAFETY: hwnd is the edit box this procedure was attached to in create_children.
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(edit_proc), EDIT_SUBCLASS) };
    }
    // SAFETY: the arguments are the ones Windows passed in for this subclassed window.
    let res = unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
    if msg == WM_GETDLGCODE {
        let keys = DLGC_WANTALLKEYS | DLGC_WANTTAB | DLGC_HASSETSEL;
        return LRESULT(res.0 & !isize::try_from(keys).unwrap_or(0));
    }
    res
}

fn create_children(hwnd: HWND, st: &mut State) {
    for c in &st.children {
        let text = HSTRING::from(crlf(&c.text));
        let menu = std::ptr::without_provenance_mut(usize::try_from(c.id).unwrap_or(0));
        // SAFETY: hwnd is the live dialog window, and class and text outlive the call.
        let child = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(c.ex),
                c.class,
                &text,
                WINDOW_STYLE(c.style) | WS_CHILD | WS_VISIBLE,
                c.rect.left,
                c.rect.top,
                c.rect.right - c.rect.left,
                c.rect.bottom - c.rect.top,
                Some(hwnd),
                Some(HMENU(menu)),
                None,
                None,
            )
        };
        let Ok(child) = child else { continue };
        // SAFETY: child was just created and the font lives until the dialog closes.
        unsafe {
            SendMessageW(
                child,
                WM_SETFONT,
                Some(WPARAM(c.font.0.addr())),
                Some(LPARAM(1)),
            );
        }
        if c.id == ID_DETAIL {
            // SAFETY: child is a live edit box, and edit_proc removes itself when the box is destroyed.
            unsafe {
                SendMessageW(child, EM_SETLIMITTEXT, Some(WPARAM(0)), Some(LPARAM(0)));
                let _ = SetWindowTextW(child, &text);
                let _ = SetWindowSubclass(child, Some(edit_proc), EDIT_SUBCLASS, 0);
            }
        }
        if c.class != w!("BUTTON") {
            st.statics.push((child, c.role));
        }
        if matches!(c.role, Role::Footer) {
            st.footer = Some(child);
        }
        if c.id == ID_NO {
            // SAFETY: child is a live window on this thread.
            unsafe {
                let _ = SetFocus(Some(child));
            }
        }
    }
}

pub(super) unsafe extern "system" fn proc_(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if msg == WM_CREATE {
        let create = std::ptr::with_exposed_provenance::<CREATESTRUCTW>(lp.0.cast_unsigned());
        // SAFETY: for WM_CREATE the LPARAM points at the CREATESTRUCTW of this window.
        let state = unsafe { (*create).lpCreateParams };
        // SAFETY: hwnd is the window being created, and the slot holds the State pointer from show.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state.expose_provenance().cast_signed()) };
    }
    // SAFETY: hwnd is the live window Windows is delivering this message to.
    let slot = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    let ptr = std::ptr::with_exposed_provenance_mut::<State>(slot.cast_unsigned());
    if ptr.is_null() {
        // SAFETY: the arguments are the ones Windows passed in for this window.
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    // SAFETY: the slot holds the State that show keeps alive until its message loop ends.
    let st = unsafe { &mut *ptr };
    match msg {
        WM_CREATE => {
            create_children(hwnd, st);
            // SAFETY: hwnd is this live window; finish kills the timer.
            unsafe { SetTimer(Some(hwnd), TIMER, 250, None) };
            LRESULT(0)
        }
        WM_TIMER => {
            match st
                .timeout
                .checked_sub(st.started.elapsed())
                .filter(|d| !d.is_zero())
            {
                None => {
                    st.timed_out = true;
                    finish(hwnd, st, ID_NO);
                }
                Some(left) => {
                    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
                    if secs != st.shown_secs {
                        st.shown_secs = secs;
                        if let Some(f) = st.footer {
                            let text = HSTRING::from(countdown_text(left));
                            // SAFETY: f is the footer label, a live child of this window.
                            unsafe {
                                let _ = SetWindowTextW(f, &text);
                            }
                        }
                    }
                }
            }
            LRESULT(0)
        }
        DM_GETDEFID => LRESULT((DC_HASDEFID << 16) | isize::try_from(ID_NO).unwrap_or(0)),
        WM_COMMAND => {
            match i32::try_from(wp.0 & 0xFFFF).unwrap_or(0) {
                id @ (ID_GRANT | ID_ONCE | ID_NO) => finish(hwnd, st, id),
                id if id == IDCANCEL.0 || id == IDNO.0 => finish(hwnd, st, ID_NO),
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            finish(hwnd, st, ID_NO);
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let dc = HDC(std::ptr::with_exposed_provenance_mut(wp.0));
            let child = HWND(std::ptr::with_exposed_provenance_mut(lp.0.cast_unsigned()));
            let role = st
                .statics
                .iter()
                .find(|(h, _)| *h == child)
                .map_or(Role::Plain, |(_, r)| *r);
            // SAFETY: for WM_CTLCOLORSTATIC the WPARAM is the child's live device context.
            unsafe {
                match role {
                    Role::Footer => {
                        SetBkColor(dc, BAND);
                        LRESULT(st.band.0.expose_provenance().cast_signed())
                    }
                    Role::Big => {
                        SetTextColor(dc, BLUE);
                        SetBkColor(dc, WHITE);
                        LRESULT(st.white.0.expose_provenance().cast_signed())
                    }
                    Role::Plain => {
                        SetBkColor(dc, WHITE);
                        LRESULT(st.white.0.expose_provenance().cast_signed())
                    }
                }
            }
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: ps is a live local, and EndPaint closes the paint BeginPaint opened on hwnd.
            unsafe {
                let dc = BeginPaint(hwnd, &mut ps);
                FillRect(dc, &rect(0, 0, st.width, st.band_top), st.white);
                FillRect(
                    dc,
                    &rect(0, st.band_top, st.width, st.height - st.band_top),
                    st.band,
                );
                FillRect(dc, &rect(0, st.band_top, st.width, 1), st.line);
                FillRect(dc, &rect(0, st.footer_top, st.width, 1), st.line);
                if let Ok(icon) = LoadIconW(None, IDI_WARNING) {
                    let (x, y, s) = st.icon_at;
                    let _ = DrawIconEx(dc, x, y, icon, s, s, 0, None, DI_NORMAL);
                }
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        // SAFETY: the arguments are the ones Windows passed in for this window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn finish(hwnd: HWND, st: &mut State, id: i32) {
    if st.done {
        return;
    }
    st.result = Some(id);
    st.done = true;
    // SAFETY: hwnd is the live dialog window, and st.done stops a second call.
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER);
        let _ = DestroyWindow(hwnd);
    }
}

use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use windows::core::{w, BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontIndirectW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetDC, GetDeviceCaps, ReleaseDC, SelectObject, SetBkColor, SetTextColor, DT_CALCRECT,
    DT_EDITCONTROL, DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK, HBRUSH, HDC, HFONT, HGDIOBJ,
    LOGFONTW, LOGPIXELSY, PAINTSTRUCT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, DrawIconEx, EnumThreadWindows, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, IsDialogMessageW, KillTimer, LoadCursorW, LoadIconW, MessageBoxW,
    PostMessageW, RegisterClassExW, SendMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    SetWindowTextW, SystemParametersInfoW, TranslateMessage, CREATESTRUCTW, DI_NORMAL,
    DLGC_HASSETSEL, DLGC_WANTALLKEYS, DLGC_WANTTAB, GWLP_USERDATA, GWLP_WNDPROC, HMENU, IDCANCEL,
    IDC_ARROW, IDI_WARNING, IDNO, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    MSG, NONCLIENTMETRICSW, SM_CYEDGE, SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_CTLCOLORSTATIC, WM_GETDLGCODE, WM_PAINT, WM_SETFONT, WM_TIMER, WM_USER,
    WNDCLASSEXW, WNDPROC, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME,
    WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};

use super::{
    countdown_text, prompt_parts, prompt_text, Answer, ConsentError, Request, Term, PROMPT_TIMEOUT,
    PROMPT_TITLE,
};

pub const ID_GRANT: i32 = 1001;
pub const ID_ONCE: i32 = 1002;
pub const ID_NO: i32 = 1003;
pub const ID_DETAIL: i32 = 1020;

pub const BOX_ROWS_MAX: i32 = 14;
pub const BOX_ROWS_MIN: i32 = 3;

const SS_NOPREFIX: u32 = 0x80;
const SS_EDITCONTROL: u32 = 0x2000;
const ES_MULTILINE: u32 = 0x4;
const ES_READONLY: u32 = 0x800;
const BS_DEFPUSHBUTTON: u32 = 0x1;
const EM_SETLIMITTEXT: u32 = 0xC5;
const DM_GETDEFID: u32 = WM_USER;
const DC_HASDEFID: isize = 0x534B;
const TIMER: usize = 7;

const WHITE: COLORREF = COLORREF(0x00FF_FFFF);
const BAND: COLORREF = COLORREF(0x00F0_F0F0);
const LINE: COLORREF = COLORREF(0x00DF_DFDF);
const BLUE: COLORREF = COLORREF(0x0099_3300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

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

pub fn work_area() -> Area {
    let mut r = RECT::default();
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut r as *mut RECT as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    Area {
        left: r.left,
        top: r.top,
        width: r.right - r.left,
        height: r.bottom - r.top,
    }
}

struct Fonts {
    body: HFONT,
    big: HFONT,
    mono: HFONT,
    dpi: i32,
}

impl Fonts {
    fn new(dpi: Option<i32>) -> Fonts {
        let mut m = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let _ = unsafe {
            SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                m.cbSize,
                Some(&mut m as *mut NONCLIENTMETRICSW as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        let system = unsafe {
            let dc = GetDC(None);
            let d = GetDeviceCaps(Some(dc), LOGPIXELSY);
            ReleaseDC(None, dc);
            d.max(1)
        };
        let dpi = dpi.unwrap_or(system).max(1);
        let points = |pt: f32| -((pt * dpi as f32 / 72.0).round() as i32);
        let mut body_lf = m.lfMessageFont;
        if body_lf.lfHeight == 0 {
            body_lf.lfHeight = points(9.0);
        } else {
            body_lf.lfHeight = body_lf.lfHeight * dpi / system;
        }
        let mut big_lf = body_lf;
        big_lf.lfHeight = points(12.0);
        let mut mono_lf = LOGFONTW {
            lfHeight: points(9.0),
            ..Default::default()
        };
        for (i, c) in "Consolas".encode_utf16().enumerate() {
            mono_lf.lfFaceName[i] = c;
        }
        unsafe {
            Fonts {
                body: CreateFontIndirectW(&body_lf),
                big: CreateFontIndirectW(&big_lf),
                mono: CreateFontIndirectW(&mono_lf),
                dpi,
            }
        }
    }

    fn px(&self, v: i32) -> i32 {
        (v * self.dpi + 48) / 96
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.body.0));
            let _ = DeleteObject(HGDIOBJ(self.big.0));
            let _ = DeleteObject(HGDIOBJ(self.mono.0));
        }
    }
}

pub fn measure(font: HFONT, text: &str, width: i32, single: bool) -> (i32, i32) {
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    if buf.is_empty() {
        return (0, 0);
    }
    let mut r = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: 0,
    };
    let flags = if single {
        DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX
    } else {
        DT_CALCRECT | DT_WORDBREAK | DT_EDITCONTROL | DT_NOPREFIX
    };
    unsafe {
        let dc = GetDC(None);
        let old = SelectObject(dc, HGDIOBJ(font.0));
        DrawTextW(dc, &mut buf, &mut r, flags);
        SelectObject(dc, old);
        ReleaseDC(None, dc);
    }
    (r.right - r.left, r.bottom - r.top)
}

pub fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

#[derive(Clone, Copy)]
enum Role {
    Plain,
    Big,
    Footer,
}

struct Child {
    class: PCWSTR,
    text: String,
    style: u32,
    ex: u32,
    id: i32,
    rect: RECT,
    font: HFONT,
    role: Role,
}

fn label(text: &str, rect: RECT, font: HFONT, role: Role) -> Child {
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

struct State {
    children: Vec<Child>,
    statics: Vec<(HWND, Role)>,
    band_top: i32,
    footer_top: i32,
    width: i32,
    height: i32,
    icon_at: (i32, i32, i32),
    timeout: Duration,
    started: Instant,
    shown_secs: u64,
    footer: Option<HWND>,
    result: Option<i32>,
    timed_out: bool,
    done: bool,
    white: HBRUSH,
    band: HBRUSH,
    line: HBRUSH,
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.white.0));
            let _ = DeleteObject(HGDIOBJ(self.band.0));
            let _ = DeleteObject(HGDIOBJ(self.line.0));
        }
    }
}

static EDIT_PROC: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let old: WNDPROC = unsafe { std::mem::transmute(EDIT_PROC.load(Ordering::SeqCst)) };
    let res = unsafe { CallWindowProcW(old, hwnd, msg, wp, lp) };
    if msg == WM_GETDLGCODE {
        return LRESULT(res.0 & !((DLGC_WANTALLKEYS | DLGC_WANTTAB | DLGC_HASSETSEL) as isize));
    }
    res
}

fn create_children(hwnd: HWND, st: &mut State) {
    for c in &st.children {
        let text = HSTRING::from(crlf(&c.text));
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
                Some(HMENU(c.id as isize as *mut _)),
                None,
                None,
            )
        };
        let Ok(child) = child else { continue };
        unsafe {
            SendMessageW(
                child,
                WM_SETFONT,
                Some(WPARAM(c.font.0 as usize)),
                Some(LPARAM(1)),
            );
        }
        if c.id == ID_DETAIL {
            unsafe {
                SendMessageW(child, EM_SETLIMITTEXT, Some(WPARAM(0)), Some(LPARAM(0)));
                let _ = SetWindowTextW(child, &text);
                let ours = edit_proc as *const () as isize;
                let old = SetWindowLongPtrW(child, GWLP_WNDPROC, ours);
                if old != 0 && old != ours {
                    EDIT_PROC.store(old, Ordering::SeqCst);
                }
            }
        }
        if c.class != w!("BUTTON") {
            st.statics.push((child, c.role));
        }
        if matches!(c.role, Role::Footer) {
            st.footer = Some(child);
        }
        if c.id == ID_NO {
            unsafe {
                let _ = SetFocus(Some(child));
            }
        }
    }
}

unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_CREATE {
        let cs = unsafe { &*(lp.0 as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
    }
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    let st = unsafe { &mut *ptr };
    match msg {
        WM_CREATE => {
            create_children(hwnd, st);
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
                            unsafe {
                                let _ = SetWindowTextW(f, &text);
                            }
                        }
                    }
                }
            }
            LRESULT(0)
        }
        DM_GETDEFID => LRESULT((DC_HASDEFID << 16) | ID_NO as isize),
        WM_COMMAND => {
            match (wp.0 & 0xFFFF) as i32 {
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
            let dc = HDC(wp.0 as *mut _);
            let child = HWND(lp.0 as *mut _);
            let role = st
                .statics
                .iter()
                .find(|(h, _)| *h == child)
                .map_or(Role::Plain, |(_, r)| *r);
            unsafe {
                match role {
                    Role::Footer => {
                        SetBkColor(dc, BAND);
                        LRESULT(st.band.0 as isize)
                    }
                    Role::Big => {
                        SetTextColor(dc, BLUE);
                        SetBkColor(dc, WHITE);
                        LRESULT(st.white.0 as isize)
                    }
                    Role::Plain => {
                        SetBkColor(dc, WHITE);
                        LRESULT(st.white.0 as isize)
                    }
                }
            }
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
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
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn finish(hwnd: HWND, st: &mut State, id: i32) {
    if st.done {
        return;
    }
    st.result = Some(id);
    st.done = true;
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER);
        let _ = DestroyWindow(hwnd);
    }
}

const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_POPUP.0 | WS_CAPTION.0 | WS_SYSMENU.0);

fn chrome() -> (i32, i32) {
    let mut r = rect(0, 0, 100, 100);
    let _ = unsafe { AdjustWindowRectEx(&mut r, STYLE, false, WS_EX_DLGMODALFRAME) };
    ((r.right - r.left) - 100, (r.bottom - r.top) - 100)
}

fn button_row(f: &Fonts, labels: &[(i32, String)], right: i32, y: i32) -> Vec<Child> {
    let h = f.px(26);
    let gap = f.px(8);
    let mut x = right;
    let mut out = Vec::new();
    for (id, text) in labels.iter().rev() {
        let (tw, _) = measure(f.body, text, 10_000, true);
        let w = (tw + f.px(32)).max(f.px(96));
        x -= w;
        let mut style = WS_TABSTOP.0;
        if *id == ID_NO {
            style |= BS_DEFPUSHBUTTON;
        }
        out.push(Child {
            class: w!("BUTTON"),
            text: text.clone(),
            style,
            ex: 0,
            id: *id,
            rect: rect(x, y, w, h),
            font: f.body,
            role: Role::Plain,
        });
        x -= gap;
    }
    out.reverse();
    out
}

struct Layout {
    children: Vec<Child>,
    width: i32,
    height: i32,
    band_top: i32,
    footer_top: i32,
}

fn layout(f: &Fonts, req: &Request, area: Area, timeout: Duration) -> Layout {
    let parts = prompt_parts(req);
    let (chrome_w, chrome_h) = chrome();
    let width = f
        .px(620)
        .min(area.width - chrome_w - f.px(16))
        .max(f.px(360));
    let x0 = f.px(56);
    let tw = width - x0 - f.px(20);
    let (_, line_h) = measure(f.mono, "Xy", 10_000, true);
    let line_h = line_h.max(1);
    let edge = 2 * unsafe { GetSystemMetrics(SM_CYEDGE) };
    let inner = tw - f.px(30);
    let scope = parts.scope_line();
    let (_, ih) = measure(f.big, &parts.instruction, tw, false);
    let (_, sh) = measure(f.body, &scope, tw, false);
    let (_, ch) = measure(f.body, &parts.caution, tw, false);
    let band_h = f.px(46);
    let footer_h = f.px(34);
    let below = f.px(10) + sh + f.px(8) + ch + f.px(16) + band_h + footer_h;
    let room_for = |head_h: i32| {
        let top = f.px(14) + ih + f.px(10) + if head_h > 0 { head_h + f.px(8) } else { 0 };
        (area.height - f.px(24) - chrome_h - top - below - edge) / line_h
    };

    let mut head = parts.summary.clone();
    let mut body = parts.detail.clone().unwrap_or_default();
    let mut hh = measure(f.body, &head, tw, false).1;
    if !head.is_empty() && room_for(hh) < BOX_ROWS_MIN {
        body = if body.is_empty() {
            head.clone()
        } else {
            format!("{head}\n\n{body}")
        };
        head.clear();
        hh = 0;
    }
    let wanted = if body.is_empty() {
        0
    } else {
        let (_, bh) = measure(f.mono, &crlf(&body), inner, false);
        ((bh + line_h - 1) / line_h).max(1)
    };
    let rows = wanted.min(BOX_ROWS_MAX).min(room_for(hh).max(BOX_ROWS_MIN));

    let mut kids = Vec::new();
    let mut y = f.px(14);
    kids.push(label(
        &parts.instruction,
        rect(x0, y, tw, ih),
        f.big,
        Role::Big,
    ));
    y += ih + f.px(10);
    if !head.is_empty() {
        kids.push(label(&head, rect(x0, y, tw, hh), f.body, Role::Plain));
        y += hh + f.px(8);
    }
    if rows > 0 {
        let box_h = rows * line_h + edge;
        kids.push(Child {
            class: w!("EDIT"),
            text: body,
            style: ES_MULTILINE | ES_READONLY | WS_VSCROLL.0 | WS_TABSTOP.0,
            ex: WS_EX_CLIENTEDGE.0,
            id: ID_DETAIL,
            rect: rect(x0, y, tw, box_h),
            font: f.mono,
            role: Role::Plain,
        });
        y += box_h;
    }
    y += f.px(10);
    kids.push(label(&scope, rect(x0, y, tw, sh), f.body, Role::Plain));
    y += sh + f.px(8);
    kids.push(label(
        &parts.caution,
        rect(x0, y, tw, ch),
        f.body,
        Role::Plain,
    ));
    y += ch + f.px(16);

    let band_top = y;
    let mut labels = Vec::new();
    if let Some(g) = &parts.grant_button {
        labels.push((ID_GRANT, g.clone()));
    }
    labels.push((ID_ONCE, "Just this once".to_string()));
    labels.push((ID_NO, "Don't allow".to_string()));
    kids.extend(button_row(
        f,
        &labels,
        width - f.px(12),
        band_top + f.px(10),
    ));
    let footer_top = band_top + band_h;
    let footer = countdown_text(timeout);
    let (_, fh) = measure(f.body, &footer, tw, true);
    kids.push(Child {
        class: w!("STATIC"),
        text: footer,
        style: SS_NOPREFIX,
        ex: 0,
        id: 0,
        rect: rect(
            f.px(16),
            footer_top + (footer_h - fh) / 2,
            width - f.px(32),
            fh,
        ),
        font: f.body,
        role: Role::Footer,
    });
    Layout {
        children: kids,
        width,
        height: footer_top + footer_h,
        band_top,
        footer_top,
    }
}

pub fn show(req: &Request, opts: &Options) -> Result<Answer, ConsentError> {
    let f = Fonts::new(opts.dpi);
    let area = opts.area.unwrap_or_else(work_area);
    let lay = layout(&f, req, area, opts.timeout);
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
        white: unsafe { CreateSolidBrush(WHITE) },
        band: unsafe { CreateSolidBrush(BAND) },
        line: unsafe { CreateSolidBrush(LINE) },
    }));

    let class = w!("HandoffConsentPrompt");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(proc_),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: class,
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wc) };
    let ex = if opts.steal_focus {
        WS_EX_DLGMODALFRAME | WS_EX_TOPMOST
    } else {
        WS_EX_DLGMODALFRAME
    };
    let mut r = rect(0, 0, lay.width, lay.height);
    let _ = unsafe { AdjustWindowRectEx(&mut r, STYLE, false, ex) };
    let (ow, oh) = (r.right - r.left, r.bottom - r.top);
    let x = area.left + (area.width - ow).max(0) / 2;
    let y = area.top + (area.height - oh).max(0) / 2;
    let title = HSTRING::from(PROMPT_TITLE);
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
            Some(st as *const _),
        )
    };
    let hwnd = match created {
        Ok(h) => h,
        Err(_) => {
            drop(unsafe { Box::from_raw(st) });
            return message_box(req, opts);
        }
    };
    if opts.steal_focus {
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
    }
    let mut msg = MSG::default();
    loop {
        if unsafe { (*st).done } {
            break;
        }
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if got.0 <= 0 {
            break;
        }
        unsafe {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    if !unsafe { (*st).done } {
        unsafe {
            (*st).done = true;
            let _ = DestroyWindow(hwnd);
        }
    }
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
        let _ = unsafe { PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(IDNO.0 as usize), LPARAM(0)) };
        BOOL(1)
    }
    unsafe {
        let _ = EnumThreadWindows(thread_id, Some(press_no), LPARAM(0));
    }
}

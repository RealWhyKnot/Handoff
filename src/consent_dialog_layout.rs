use super::dialog::BOX_ROWS_MAX;
use super::dialog::BOX_ROWS_MIN;
use super::dialog::BS_DEFPUSHBUTTON;
use super::dialog::ES_MULTILINE;
use super::dialog::ES_READONLY;
use super::dialog::ID_DETAIL;
use super::dialog::ID_GRANT;
use super::dialog::ID_NO;
use super::dialog::ID_ONCE;
use super::dialog::SS_NOPREFIX;
use super::dialog_proc::Child;
use super::dialog_proc::Role;
use super::dialog_proc::label;
use super::{Request, countdown_text, prompt_parts};
use std::time::Duration;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DT_CALCRECT, DT_EDITCONTROL, DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK,
    DeleteObject, DrawTextW, GetDC, GetDeviceCaps, HFONT, HGDIOBJ, LOGFONTW, LOGPIXELSY, ReleaseDC,
    SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, GetSystemMetrics, NONCLIENTMETRICSW, SM_CYEDGE, SPI_GETNONCLIENTMETRICS,
    SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW, WINDOW_STYLE,
    WS_CAPTION, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME, WS_POPUP, WS_SYSMENU, WS_TABSTOP,
    WS_VSCROLL,
};
use windows::core::w;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

pub fn work_area() -> Area {
    let mut r = RECT::default();
    // SAFETY: r is a live RECT, the type SPI_GETWORKAREA fills in.
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(std::ptr::from_mut(&mut r).cast()),
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

pub(super) struct Fonts {
    body: HFONT,
    big: HFONT,
    mono: HFONT,
    dpi: i32,
}

impl Fonts {
    pub(super) fn new(dpi: Option<i32>) -> Fonts {
        let mut m = NONCLIENTMETRICSW {
            cbSize: u32::try_from(std::mem::size_of::<NONCLIENTMETRICSW>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: m is a live NONCLIENTMETRICSW and cbSize holds its size.
        let _ = unsafe {
            SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                m.cbSize,
                Some(std::ptr::from_mut(&mut m).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        // SAFETY: the screen DC from GetDC is released before the block ends.
        let system = unsafe {
            let dc = GetDC(None);
            let d = GetDeviceCaps(Some(dc), LOGPIXELSY);
            ReleaseDC(None, dc);
            d.max(1)
        };
        let dpi = dpi.unwrap_or(system).max(1);
        let points = |pt: i32| -((pt * dpi * 2 + 72) / 144);
        let mut body_lf = m.lfMessageFont;
        if body_lf.lfHeight == 0 {
            body_lf.lfHeight = points(9);
        } else {
            body_lf.lfHeight = body_lf.lfHeight * dpi / system;
        }
        let mut big_lf = body_lf;
        big_lf.lfHeight = points(12);
        let mut mono_lf = LOGFONTW {
            lfHeight: points(9),
            ..Default::default()
        };
        for (i, c) in "Consolas".encode_utf16().enumerate() {
            mono_lf.lfFaceName[i] = c;
        }
        // SAFETY: each LOGFONTW is a live local for the length of its call.
        unsafe {
            Fonts {
                body: CreateFontIndirectW(&body_lf),
                big: CreateFontIndirectW(&big_lf),
                mono: CreateFontIndirectW(&mono_lf),
                dpi,
            }
        }
    }

    pub(super) fn px(&self, v: i32) -> i32 {
        (v * self.dpi + 48) / 96
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        // SAFETY: the three fonts were made in Fonts::new and are deleted only here.
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
    // SAFETY: buf and r are live locals, and the screen DC gets its old font back before release.
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

pub(super) fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

pub(super) const STYLE: WINDOW_STYLE = WINDOW_STYLE(WS_POPUP.0 | WS_CAPTION.0 | WS_SYSMENU.0);

fn chrome() -> (i32, i32) {
    let mut r = rect(0, 0, 100, 100);
    // SAFETY: r is a live RECT the call adjusts in place.
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

pub(super) struct Layout {
    pub(super) children: Vec<Child>,
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) band_top: i32,
    pub(super) footer_top: i32,
}

pub(super) fn layout(f: &Fonts, req: &Request, area: Area, timeout: Duration) -> Layout {
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
    // SAFETY: GetSystemMetrics takes an index only.
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

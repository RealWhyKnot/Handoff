use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, GetWindowRect, GetWindowTextW, IsWindowVisible,
};
use windows::core::BOOL;

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
unsafe extern "system" {
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
unsafe extern "system" {
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

pub(crate) fn out_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("ux-preview")
        .join("dialog");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(super) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn find_window(thread: u32, title: &str) -> Option<HWND> {
    struct Search {
        title: String,
        found: Option<HWND>,
    }
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
        let ptr = std::ptr::with_exposed_provenance_mut::<Search>(lp.0.cast_unsigned());
        // SAFETY: find_window passes a Search that outlives the enumeration.
        let search = unsafe { &mut *ptr };
        let mut buf = [0u16; 256];
        // SAFETY: hwnd is a live window handed to this callback and buf is a live buffer.
        let n = usize::try_from(unsafe { GetWindowTextW(hwnd, &mut buf) }).unwrap_or(0);
        let text = String::from_utf16_lossy(&buf[..n]);
        // SAFETY: hwnd is a live window handed to this callback.
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
    let target = std::ptr::from_mut(&mut search)
        .expose_provenance()
        .cast_signed();
    // SAFETY: search stays alive for the whole enumeration and each expects a Search.
    unsafe {
        let _ = EnumThreadWindows(thread, Some(each), LPARAM(target));
    }
    search.found
}

fn grab(hwnd: HWND, flags: u32) -> Option<(u32, u32, Vec<u8>)> {
    let mut rect = RECT::default();
    // SAFETY: rect is a live RECT the call fills in.
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    let (width, height) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    let raw = hwnd.0;
    // SAFETY: bgra holds four bytes per pixel, and every GDI object made here is freed before the block ends.
    unsafe {
        let screen = GetDC(raw);
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        let printed = PrintWindow(raw, mem, flags);
        let mut info = BitmapInfoHeader {
            size: u32::try_from(std::mem::size_of::<BitmapInfoHeader>()).unwrap_or(0),
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
        let mut bgra = vec![0u8; crate::util::to_usize(width) * crate::util::to_usize(height) * 4];
        let lines = GetDIBits(mem, bmp, 0, height, bgra.as_mut_ptr().cast(), &mut info, 0);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(raw, screen);
        if printed == 0 || lines != h {
            return None;
        }
        let mut rgba = Vec::with_capacity(bgra.len());
        for px in bgra.as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        let first = &rgba[..4];
        if rgba.as_chunks::<4>().0.iter().all(|p| p == first) {
            return None;
        }
        Some((width, height, rgba))
    }
}

fn save_png(path: &PathBuf, w: u32, h: u32, rgba: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(rgba).unwrap();
}

pub(crate) struct Shot {
    pub width: u32,
    pub height: u32,
    pub flags: u32,
}

pub(crate) fn capture<S, C>(name: &str, title: &str, show: S, close: C) -> Result<Shot, String>
where
    S: FnOnce() + Send + 'static,
    C: Fn(HWND) + Send + 'static,
{
    let desk_name = wide(&format!("handoff-preview-{}", std::process::id()));
    // SAFETY: desk_name is a live NUL-terminated string and the other pointers may be null.
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
    let desk_addr = desk.expose_provenance();
    let thread = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&thread);
    let shower = std::thread::spawn(move || {
        let desk = std::ptr::with_exposed_provenance_mut::<c_void>(desk_addr);
        // SAFETY: desk is the desktop created above, closed only after both threads finish.
        if unsafe { SetThreadDesktop(desk) } == 0 {
            return Err(format!(
                "SetThreadDesktop failed on the dialog thread: {}",
                std::io::Error::last_os_error()
            ));
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        // SAFETY: GetCurrentThreadId takes no arguments and cannot fail.
        seen.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        show();
        Ok(())
    });
    let path = out_dir().join(name);
    let title = title.to_string();
    let grabber = std::thread::spawn(move || -> Result<Shot, String> {
        let desk = std::ptr::with_exposed_provenance_mut::<c_void>(desk_addr);
        // SAFETY: desk is the desktop created above, closed only after both threads finish.
        if unsafe { SetThreadDesktop(desk) } == 0 {
            return Err(format!(
                "SetThreadDesktop failed on the capture thread: {}",
                std::io::Error::last_os_error()
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let hwnd = loop {
            let id = thread.load(Ordering::SeqCst);
            if id != 0
                && let Some(h) = find_window(id, &title)
            {
                break h;
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
        .map_err(|_panic| "capture thread panicked".to_string())?;
    let shown = shower
        .join()
        .map_err(|_panic| "dialog thread panicked".to_string())?;
    // SAFETY: both threads that used the desktop have been joined.
    unsafe { CloseDesktop(desk) };
    shown?;
    shot
}

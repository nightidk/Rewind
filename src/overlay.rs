use std::cell::UnsafeCell;
use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, AddFontMemResourceEx, BeginPaint, CLIP_DEFAULT_PRECIS, CreateFontW,
    CreateSolidBrush, DEFAULT_CHARSET, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetMonitorInfoW, HDC, HGDIOBJ,
    InvalidateRect, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, RemoveFontMemResourceEx, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
    UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect,
    GetWindowLongPtrW, HTTRANSPARENT, HWND_TOPMOST, LWA_ALPHA, RegisterClassW, SW_HIDE,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, WM_NCHITTEST, WM_PAINT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{Error, PCWSTR, Result, w};

use crate::localization::Locale;

#[derive(Clone, Copy)]
pub enum Notice {
    Saving,
    Saved,
    Failed,
    RecordingFailed,
}

impl Notice {
    pub fn title(self) -> &'static str {
        match self {
            Self::Saving => "Сохраняем повтор",
            Self::Saved => "Повтор сохранён",
            Self::Failed => "Повтор не сохранён",
            Self::RecordingFailed => "Ошибка записи",
        }
    }

    pub fn is_error(self) -> bool {
        matches!(self, Self::Failed | Self::RecordingFailed)
    }
}

struct Content {
    notice: Notice,
    locale: Locale,
    detail: Vec<u16>,
    dpi: i32,
}

pub struct Overlay {
    window: HWND,
    font: HANDLE,
    content: Box<UnsafeCell<Content>>,
    until: Option<Instant>,
    saving_until: Option<Instant>,
    pending: Option<(Notice, Locale, Vec<u16>)>,
}

impl Overlay {
    pub fn new() -> Result<Self> {
        let class_name = w!("Rewind.SaveOverlay");
        let instance = HINSTANCE(unsafe { GetModuleHandleW(PCWSTR::null())? }.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(Error::from_win32());
        }
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW,
                class_name,
                w!("Rewind"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )?
        };
        let bytes = include_bytes!("../ui/fonts/Montserrat.ttf");
        let mut faces = 0;
        let font = unsafe {
            AddFontMemResourceEx(
                bytes.as_ptr().cast(),
                bytes.len() as u32,
                None,
                &raw mut faces,
            )
        };
        let overlay = Self {
            window,
            font,
            content: Box::new(UnsafeCell::new(Content {
                notice: Notice::Saving,
                locale: Locale::Ru,
                detail: Vec::new(),
                dpi: 96,
            })),
            until: None,
            saving_until: None,
            pending: None,
        };
        // The boxed state stays at a stable address on the event loop's window thread.
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, overlay.content.get() as isize);
            SetLayeredWindowAttributes(window, COLORREF(0), 255, LWA_ALPHA)?;
        }
        Ok(overlay)
    }

    pub fn show(&mut self, notice: Notice, locale: Locale, detail: &str) -> Result<()> {
        let now = Instant::now();
        let detail = detail
            .lines()
            .next()
            .unwrap_or_default()
            .encode_utf16()
            .collect();
        if !matches!(notice, Notice::Saving) && self.saving_until.is_some_and(|until| now < until) {
            self.pending = Some((notice, locale, detail));
            return Ok(());
        }
        // The primary monitor contains the origin of the Windows virtual desktop.
        let monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetMonitorInfoW(monitor, &mut info).ok()? };
        let (mut dpi_x, mut dpi_y) = (96, 96);
        let dpi = if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
            .is_ok()
        {
            dpi_x as i32
        } else {
            96
        };
        self.content.get_mut().dpi = dpi;
        let margin = 24 * dpi / 96;
        let width = (350 * dpi / 96).min(info.rcWork.right - info.rcWork.left - 2 * margin);
        let height = 72 * dpi / 96;
        let work_height = info.rcWork.bottom - info.rcWork.top;
        let top_offset = (work_height / 8)
            .max(80 * dpi / 96)
            .min(work_height - height);
        unsafe {
            SetWindowPos(
                self.window,
                Some(HWND_TOPMOST),
                info.rcWork.right - width - margin,
                info.rcWork.top + top_offset,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )?;
        }
        self.pending = None;
        // Give the start notice time to be read even when an H.264 export finishes immediately.
        self.saving_until =
            matches!(notice, Notice::Saving).then_some(now + Duration::from_millis(800));
        self.render(notice, locale, detail, now);
        Ok(())
    }

    fn render(&mut self, notice: Notice, locale: Locale, detail: Vec<u16>, now: Instant) {
        let content = self.content.get_mut();
        content.notice = notice;
        content.locale = locale;
        content.detail = detail;
        unsafe {
            let _ = InvalidateRect(Some(self.window), None, false);
            let _ = UpdateWindow(self.window);
        }
        self.until = Some(now + Duration::from_secs(4));
    }

    pub fn tick(&mut self, now: Instant) {
        if self.saving_until.is_some_and(|until| now >= until) {
            self.saving_until = None;
            if let Some((notice, locale, detail)) = self.pending.take() {
                self.render(notice, locale, detail, now);
            }
        }
        if self.until.is_some_and(|until| now >= until) {
            unsafe {
                let _ = ShowWindow(self.window, SW_HIDE);
            }
            self.until = None;
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            SetWindowLongPtrW(self.window, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.window);
            if !self.font.is_invalid() {
                let _ = RemoveFontMemResourceEx(self.font);
            }
        }
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = unsafe { BeginPaint(window, &mut ps) };
            let content = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const Content;
            if let Some(content) = unsafe { content.as_ref() } {
                let mut bounds = RECT::default();
                if unsafe { GetClientRect(window, &mut bounds) }.is_ok() {
                    paint(dc, bounds, content);
                }
            }
            unsafe {
                let _ = EndPaint(window, &ps);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wp, lp) },
    }
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(u32::from_le_bytes([r, g, b, 0]))
}

fn paint(dc: HDC, bounds: RECT, content: &Content) {
    let scale = |pixels| pixels * content.dpi / 96;
    let accent = if content.notice.is_error() {
        rgb(255, 117, 129)
    } else {
        rgb(52, 213, 154)
    };
    let stripe = RECT {
        left: 0,
        right: scale(4),
        top: 0,
        bottom: bounds.bottom,
    };
    unsafe {
        let background = CreateSolidBrush(rgb(16, 17, 19));
        let border = CreateSolidBrush(rgb(55, 59, 62));
        let stripe_brush = CreateSolidBrush(accent);
        FillRect(dc, &bounds, background);
        FrameRect(dc, &bounds, border);
        FillRect(dc, &stripe, stripe_brush);
        let _ = DeleteObject(background.into());
        let _ = DeleteObject(border.into());
        let _ = DeleteObject(stripe_brush.into());
        SetBkMode(dc, TRANSPARENT);
    }
    let line = |top, bottom| RECT {
        left: scale(18),
        right: bounds.right - scale(16),
        top: scale(top),
        bottom: scale(bottom),
    };
    let mut title: Vec<_> = content
        .locale
        .text(content.notice.title())
        .encode_utf16()
        .collect();
    let mut detail = content.detail.clone();
    draw_line(
        dc,
        &mut title,
        line(10, 37),
        scale(17),
        600,
        rgb(249, 249, 250),
    );
    draw_line(
        dc,
        &mut detail,
        line(37, 60),
        scale(13),
        400,
        rgb(186, 192, 197),
    );
}

fn overlay_font(size: i32, weight: i32) -> HGDIOBJ {
    HGDIOBJ(unsafe {
        CreateFontW(
            -size,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            w!("Montserrat"),
        )
        .0
    })
}

fn draw_line(dc: HDC, text: &mut [u16], mut bounds: RECT, size: i32, weight: i32, color: COLORREF) {
    unsafe {
        let font = overlay_font(size, weight);
        let previous = SelectObject(dc, font);
        SetTextColor(dc, color);
        DrawTextW(
            dc,
            text,
            &mut bounds,
            DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS | DT_VCENTER,
        );
        if !previous.is_invalid() {
            SelectObject(dc, previous);
        }
        let _ = DeleteObject(font);
    }
}

#[test]
#[ignore = "briefly shows a real desktop overlay"]
fn overlay_stays_above_the_desktop_without_activating() -> Result<()> {
    use windows::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC, GetTextFaceW};
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromWindow};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetForegroundWindow, GetWindowDisplayAffinity, GetWindowRect, IsWindowVisible,
        WDA_NONE,
    };

    let foreground = unsafe { GetForegroundWindow() };
    let mut overlay = Overlay::new()?;
    assert!(!overlay.font.is_invalid());
    let mut face = [0u16; 64];
    unsafe {
        let dc = CreateCompatibleDC(None);
        let font = overlay_font(17, 600);
        let previous = SelectObject(dc, font);
        GetTextFaceW(dc, Some(&mut face));
        SelectObject(dc, previous);
        let _ = DeleteObject(font);
        let _ = DeleteDC(dc);
    }
    let face = String::from_utf16_lossy(&face);
    assert!(face.starts_with("Montserrat"), "Rendered font: {face}");
    overlay.show(Notice::Saving, Locale::Ru, "Rewind · MP4 / AV1")?;
    let style = unsafe { GetWindowLongPtrW(overlay.window, GWL_EXSTYLE) } as u32;
    let required = WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW;
    assert_eq!(style & required.0, required.0);
    assert!(unsafe { IsWindowVisible(overlay.window) }.as_bool());
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    let primary = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
    let displayed_on = unsafe { MonitorFromWindow(overlay.window, MONITOR_DEFAULTTONULL) };
    assert_eq!(displayed_on, primary);
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(primary, &mut info).ok()? };
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(overlay.window, &mut bounds)? };
    assert!(bounds.top >= info.rcWork.top + (info.rcWork.bottom - info.rcWork.top) / 8);
    assert!(bounds.bottom <= info.rcWork.bottom);
    let mut affinity = u32::MAX;
    unsafe { GetWindowDisplayAffinity(overlay.window, &mut affinity)? };
    assert_eq!(affinity, WDA_NONE.0);
    overlay.show(Notice::Saved, Locale::En, "Replay.mp4")?;
    assert_eq!(unsafe { GetForegroundWindow() }, foreground);
    assert!(matches!(overlay.content.get_mut().notice, Notice::Saving));
    let settled = Instant::now() + Duration::from_secs(1);
    overlay.tick(settled);
    assert!(matches!(overlay.content.get_mut().notice, Notice::Saved));
    overlay.tick(settled + Duration::from_secs(5));
    assert!(!unsafe { IsWindowVisible(overlay.window) }.as_bool());
    Ok(())
}

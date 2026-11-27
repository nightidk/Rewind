use std::cell::Cell;

use crate::{ipc, localization::Locale, recorder::Status};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::path::PathBuf;
use tauri::webview::{Color, NewWindowResponse};
use tauri::{App, PhysicalPosition, Theme, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, PtInRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_ESCAPE,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetWindowRect, HC_ACTION, HHOOK, HWND_TOPMOST, MA_NOACTIVATE, MSLLHOOKSTRUCT,
    PA_NOACTIVATE, PostMessageW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetWindowPos,
    SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx, WH_MOUSE_LL, WM_CLOSE, WM_HOTKEY,
    WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEACTIVATE, WM_POINTERACTIVATE, WM_RBUTTONDOWN,
    WM_XBUTTONDOWN,
};
use windows::core::PCWSTR;

const WIDTH: f64 = 328.0;
const HEIGHT: f64 = 224.0;
const ESCAPE_HOTKEY: i32 = 1;

thread_local! {
    static OPEN_MENU: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
}

#[derive(PartialEq, Eq, Serialize)]
struct PanelStatus {
    locale: Locale,
    phase: &'static str,
    seconds: u32,
    limit: u32,
    saving: bool,
    shortcut: Option<u32>,
}

pub struct TrayPanel {
    view: WebviewWindow,
    hwnd: HWND,
    last: Option<PanelStatus>,
    outside_clicks: Option<HHOOK>,
    escape_registered: bool,
}

impl TrayPanel {
    pub fn new(app: &App, webview_data: PathBuf) -> Result<Self> {
        let view = WebviewWindowBuilder::new(app, "tray", WebviewUrl::App("tray.html".into()))
            .title("Меню Rewind")
            .theme(Some(Theme::Dark))
            .inner_size(WIDTH, HEIGHT)
            .visible(false)
            .focusable(false)
            .focused(false)
            .decorations(false)
            .resizable(false)
            .skip_taskbar(true)
            .always_on_top(true)
            .data_directory(webview_data)
            .background_color(Color(10, 10, 11, 255))
            .on_navigation(|url| {
                url.host_str() == Some("tauri.localhost") && url.path() == "/tray.html"
            })
            .on_new_window(|_, _| NewWindowResponse::Deny)
            .disable_drag_drop_handler()
            .build()?;
        let hwnd = view.hwnd()?;
        ensure!(
            unsafe { SetWindowSubclass(hwnd, Some(panel_message), 0, 0) }.as_bool(),
            "Не удалось настроить обработку сообщений меню Rewind."
        );
        Ok(Self {
            view,
            hwnd,
            last: None,
            outside_clicks: None,
            escape_registered: false,
        })
    }

    pub fn open(&mut self, position: PhysicalPosition<f64>) -> Result<()> {
        let point = POINT {
            x: position.x as i32,
            y: position.y as i32,
        };
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetMonitorInfoW(monitor, &mut info).ok()? };
        let scale = self
            .view
            .monitor_from_point(position.x, position.y)?
            .map_or_else(
                || self.view.scale_factor().unwrap_or(1.0),
                |monitor| monitor.scale_factor(),
            );
        let bounds = panel_bounds(point, info.rcWork, scale);
        let hwnd = self.hwnd;
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_SHOWWINDOW | SWP_NOACTIVATE,
            )?;
        }
        if !self.escape_registered {
            self.escape_registered = unsafe {
                RegisterHotKey(
                    Some(hwnd),
                    ESCAPE_HOTKEY,
                    MOD_NOREPEAT,
                    u32::from(VK_ESCAPE.0),
                )
            }
            .is_ok();
        }
        if self.outside_clicks.is_none() {
            self.outside_clicks = unsafe {
                GetModuleHandleW(PCWSTR::null()).and_then(|module| {
                    SetWindowsHookExW(
                        WH_MOUSE_LL,
                        Some(dismiss_on_outside_click),
                        Some(HINSTANCE(module.0)),
                        0,
                    )
                })
            }
            .ok();
            if self.outside_clicks.is_some() {
                OPEN_MENU.set(hwnd);
            }
        }
        self.reveal();
        Ok(())
    }

    pub fn reveal(&mut self) {
        if self.view.is_visible().unwrap_or(false) {
            self.last = None;
            let _ = self.view.eval("window.rewind?.reveal();");
        }
    }

    pub fn hide(&mut self) {
        self.stop_listening();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn stop_listening(&mut self) {
        if self.escape_registered {
            let _ = unsafe { UnregisterHotKey(Some(self.hwnd), ESCAPE_HOTKEY) };
            self.escape_registered = false;
        }
        if let Some(hook) = self.outside_clicks.take() {
            OPEN_MENU.set(HWND::default());
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
    }

    pub fn update(
        &mut self,
        status: &Status<'_>,
        limit: u32,
        shortcut: Option<u32>,
        locale: Locale,
    ) {
        if !self.view.is_visible().unwrap_or(false) {
            return;
        }
        let state = PanelStatus {
            locale,
            phase: status.phase,
            seconds: status.seconds.min(f64::from(limit)) as u32,
            limit,
            saving: status.saving,
            shortcut,
        };
        if self.last.as_ref() != Some(&state) {
            ipc(&self.view, "status", &state);
            self.last = Some(state);
        }
    }
}

impl Drop for TrayPanel {
    fn drop(&mut self) {
        self.stop_listening();
        unsafe {
            let _ = RemoveWindowSubclass(self.hwnd, Some(panel_message), 0);
        }
    }
}

unsafe extern "system" fn panel_message(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    match message {
        // Windows can request activation by hover when active window tracking is enabled.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_POINTERACTIVATE => LRESULT(PA_NOACTIVATE as isize),
        WM_HOTKEY if wp.0 == ESCAPE_HOTKEY as usize => {
            let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            LRESULT(0)
        }
        _ => unsafe { DefSubclassProc(window, message, wp, lp) },
    }
}

unsafe extern "system" fn dismiss_on_outside_click(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && matches!(
            wp.0 as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        )
    {
        let window = OPEN_MENU.get();
        let mut bounds = RECT::default();
        if !window.is_invalid() && unsafe { GetWindowRect(window, &mut bounds) }.is_ok() {
            let click = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) }.pt;
            if !unsafe { PtInRect(&bounds, click) }.as_bool() {
                let _ = unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

fn panel_bounds(point: POINT, work: RECT, scale: f64) -> RECT {
    let gap = (8.0 * scale).round() as i32;
    let width = ((WIDTH * scale).round() as i32).min(work.right - work.left - 2 * gap);
    let height = ((HEIGHT * scale).round() as i32).min(work.bottom - work.top - 2 * gap);
    let below = point.y - height - gap < work.top;
    let y = if below {
        point.y + gap
    } else {
        point.y - height - gap
    };
    let left = (point.x + gap - width).clamp(work.left + gap, work.right - width - gap);
    let top = y.clamp(work.top + gap, work.bottom - height - gap);
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

#[test]
fn menu_fits_beside_tray_on_scaled_monitors() {
    let work = RECT {
        left: -2560,
        top: 0,
        right: 0,
        bottom: 1400,
    };
    let below = panel_bounds(POINT { x: -16, y: 1424 }, work, 1.5);
    assert_eq!(below.right - below.left, 492);
    assert_eq!(below.bottom, 1388);
    assert_eq!(below.right, -12);
    let above = panel_bounds(POINT { x: -2550, y: -10 }, work, 1.5);
    assert_eq!(above.left, -2548);
    assert_eq!(above.top, 12);
}

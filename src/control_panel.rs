use std::cell::Cell;
use std::mem::size_of;
use std::rc::Rc;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use tauri::webview::{Color, NewWindowResponse};
use tauri::{AppHandle, Theme, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::ffmpeg::Progress;
use crate::localization::Locale;
use crate::recorder::Status;
use crate::settings::{ExportCodec, OverlayShortcut, Settings};
use crate::window_events::DismissOutside;
use crate::{ipc, local_page, tray_panel::buffer_control};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, HWND_TOPMOST, IsWindowVisible, SW_HIDE, SWP_SHOWWINDOW,
    SetForegroundWindow, SetWindowPos, ShowWindow,
};

pub const LABEL: &str = "control";
pub const HOTKEY: i32 = 100;
pub const HOTKEY_ALT: i32 = 101;

pub fn register(window: HWND, id: i32, shortcut: OverlayShortcut) -> bool {
    if !shortcut.valid() {
        return false;
    }
    let mut modifiers = MOD_NOREPEAT;
    for (enabled, flag) in [
        (shortcut.alt, MOD_ALT),
        (shortcut.ctrl, MOD_CONTROL),
        (shortcut.shift, MOD_SHIFT),
        (shortcut.win, MOD_WIN),
    ] {
        if enabled {
            modifiers |= flag;
        }
    }
    unsafe { RegisterHotKey(Some(window), id, modifiers, shortcut.key.into()) }.is_ok()
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct QuickSettings {
    pub seconds: u32,
    pub fps: u32,
    pub export_codec: ExportCodec,
}

impl From<&Settings> for QuickSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            seconds: settings.seconds,
            fps: settings.fps,
            export_codec: settings.export_codec,
        }
    }
}

#[derive(PartialEq, Eq, Serialize)]
struct State {
    locale: Locale,
    phase: &'static str,
    seconds: u32,
    limit: u32,
    saving: bool,
    microphone_muted: bool,
    toggle_label: &'static str,
    toggle_enabled: bool,
    settings: QuickSettings,
    download: Option<Progress>,
}

#[derive(Default)]
pub struct OverlayWindow {
    hwnd: Cell<HWND>,
    previous_window: Cell<HWND>,
    outside_clicks: Cell<Option<DismissOutside>>,
}

impl OverlayWindow {
    pub fn available(&self) -> bool {
        !self.hwnd.get().is_invalid()
    }

    fn visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd.get()) }.as_bool()
    }

    pub fn toggle(&self) -> Result<()> {
        if self.visible() {
            self.hide();
            return Ok(());
        }
        let hwnd = self.hwnd.get();
        let monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        ensure!(
            unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool(),
            "Не удалось определить основной экран."
        );
        let bounds = info.rcMonitor;
        unsafe {
            self.previous_window.set(GetForegroundWindow());
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                bounds.left,
                bounds.top,
                bounds.right - bounds.left,
                bounds.bottom - bounds.top,
                SWP_SHOWWINDOW,
            )?;
            if GetForegroundWindow() != hwnd {
                let _ = SetForegroundWindow(hwnd);
            }
        }
        self.outside_clicks.set(DismissOutside::watch(hwnd).ok());
        Ok(())
    }

    fn hide(&self) {
        self.outside_clicks.set(None);
        let previous = self.previous_window.take();
        if !self.visible() {
            return;
        }
        let hwnd = self.hwnd.get();
        unsafe {
            // Hide the whole native surface even when the WebView is waiting to paint.
            let restore_focus = GetForegroundWindow() == hwnd;
            let _ = ShowWindow(hwnd, SW_HIDE);
            if restore_focus && previous != hwnd && IsWindowVisible(previous).as_bool() {
                let _ = SetForegroundWindow(previous);
            }
        }
    }
}

pub struct ControlPanel {
    view: WebviewWindow,
    window: Rc<OverlayWindow>,
    last: Option<State>,
}

impl ControlPanel {
    pub fn new(
        app: &AppHandle,
        data: std::path::PathBuf,
        window: Rc<OverlayWindow>,
    ) -> Result<Self> {
        let view = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("control.html".into()))
            .title("Оверлей Rewind")
            .theme(Some(Theme::Dark))
            .visible(false)
            .focused(false)
            .decorations(false)
            .resizable(false)
            .transparent(true)
            .shadow(false)
            .skip_taskbar(true)
            .always_on_top(true)
            .data_directory(data)
            .background_color(Color(0, 0, 0, 0))
            .on_navigation(|url| local_page(url, &["/control.html"]))
            .on_new_window(|_, _| NewWindowResponse::Deny)
            .disable_drag_drop_handler()
            .build()?;
        let disabled = 1_i32;
        let hwnd = view.hwnd()?;
        let _ = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_TRANSITIONS_FORCEDISABLED,
                (&raw const disabled).cast(),
                size_of::<i32>() as u32,
            )
        };
        window.hwnd.set(hwnd);
        Ok(Self {
            view,
            window,
            last: None,
        })
    }

    pub fn finish_toggle(&mut self) {
        if self.window.visible() {
            let _ = self.view.as_ref().show();
            self.focus_webview();
            self.refresh();
            ipc(&self.view, "opened", &true);
        } else {
            let _ = self.view.as_ref().hide();
        }
    }

    pub fn hide(&mut self) {
        self.window.hide();
        let _ = self.view.as_ref().hide();
    }

    pub fn refresh(&mut self) {
        self.last = None;
    }

    pub fn focus_webview(&self) {
        if unsafe { GetForegroundWindow() } == self.window.hwnd.get() {
            let _ = self.view.as_ref().set_focus();
        }
    }

    pub fn applied(&self, error: Option<&str>) {
        ipc(&self.view, "applied", &error);
    }

    pub fn update(
        &mut self,
        status: &Status<'_>,
        settings: &Settings,
        locale: Locale,
        download: Option<Progress>,
    ) {
        if !self.window.visible() {
            return;
        }
        let (toggle_label, toggle_enabled) = buffer_control(status.phase, status.saving);
        let limit = if status.phase == "idle" {
            settings.seconds
        } else {
            status.limit.unwrap_or(settings.seconds)
        };
        let state = State {
            locale,
            phase: status.phase,
            seconds: status.seconds.min(f64::from(limit)) as u32,
            limit,
            saving: status.saving,
            microphone_muted: status.microphone_muted,
            toggle_label,
            toggle_enabled,
            settings: QuickSettings::from(settings),
            download,
        };
        if self.last.as_ref() != Some(&state) {
            ipc(&self.view, "status", &state);
            self.last = Some(state);
        }
    }
}

use std::mem::size_of;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::webview::{Color, NewWindowResponse};
use tauri::{AppHandle, Theme, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::ffmpeg::Progress;
use crate::localization::Locale;
use crate::recorder::Status;
use crate::settings::{ExportCodec, OverlayShortcut, Settings};
use crate::{ipc, tray_panel::buffer_control};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, HWND_TOPMOST, IsWindowVisible, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
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
    toggle_label: &'static str,
    toggle_enabled: bool,
    settings: QuickSettings,
    download: Option<Progress>,
}

pub struct ControlPanel {
    view: WebviewWindow,
    hwnd: HWND,
    last: Option<State>,
    previous_window: HWND,
}

impl ControlPanel {
    pub fn new(app: &AppHandle, data: std::path::PathBuf) -> Result<Self> {
        let view = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("control.html".into()))
            .title("Оверлей Rewind")
            .theme(Some(Theme::Dark))
            .visible(false)
            .decorations(false)
            .resizable(false)
            .transparent(true)
            .shadow(false)
            .skip_taskbar(true)
            .always_on_top(true)
            .data_directory(data)
            .background_color(Color(0, 0, 0, 0))
            .on_navigation(|url| {
                url.host_str() == Some("tauri.localhost") && url.path() == "/control.html"
            })
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
        Ok(Self {
            view,
            hwnd,
            last: None,
            previous_window: HWND::default(),
        })
    }

    pub fn toggle(&mut self) -> Result<()> {
        if unsafe { IsWindowVisible(self.hwnd) }.as_bool() {
            self.hide();
        } else {
            let monitor = self
                .view
                .primary_monitor()?
                .context("Не удалось определить основной экран.")?;
            self.refresh();
            ipc(&self.view, "opened", &true);
            self.previous_window = unsafe { GetForegroundWindow() };
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    monitor.position().x,
                    monitor.position().y,
                    i32::try_from(monitor.size().width)?,
                    i32::try_from(monitor.size().height)?,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )?;
                let _ = SetForegroundWindow(self.hwnd);
            }
        }
        Ok(())
    }

    pub fn hide(&mut self) {
        let previous = std::mem::take(&mut self.previous_window);
        unsafe {
            if !IsWindowVisible(self.hwnd).as_bool() {
                return;
            }
            // Hide the whole native surface even when the WebView is waiting to paint.
            let restore_focus = GetForegroundWindow() == self.hwnd;
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            if restore_focus && previous != self.hwnd && IsWindowVisible(previous).as_bool() {
                let _ = SetForegroundWindow(previous);
            }
        }
    }

    pub fn refresh(&mut self) {
        self.last = None;
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
        if !unsafe { IsWindowVisible(self.hwnd) }.as_bool() {
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

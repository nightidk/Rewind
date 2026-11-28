use std::mem::size_of;
use std::time::{Duration, Instant};

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
    last: Option<State>,
    close_serial: u32,
    closing: Option<(u32, Instant)>,
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
            last: None,
            close_serial: 0,
            closing: None,
        })
    }

    pub fn toggle(&mut self) -> Result<()> {
        if self.view.is_visible()? {
            self.hide();
        } else {
            let monitor = self
                .view
                .primary_monitor()?
                .context("Не удалось определить основной экран.")?;
            self.view.set_position(*monitor.position())?;
            self.view.set_size(*monitor.size())?;
            self.refresh();
            ipc(&self.view, "opened", &true);
            self.view.show()?;
            let _ = self.view.set_focus();
        }
        Ok(())
    }

    pub fn hide(&mut self) {
        if self.closing.is_some() || !self.view.is_visible().unwrap_or(false) {
            return;
        }
        self.close_serial = self.close_serial.wrapping_add(1);
        self.closing = Some((
            self.close_serial,
            Instant::now() + Duration::from_millis(300),
        ));
        ipc(&self.view, "dismiss", &self.close_serial);
    }

    pub fn finish_hide(&mut self, close_id: u32) {
        if self.closing.is_some_and(|(id, _)| id == close_id) {
            self.closing = None;
            let _ = self.view.hide();
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
        if let Some((id, deadline)) = self.closing
            && Instant::now() >= deadline
        {
            self.finish_hide(id);
        }
        if !self.view.is_visible().unwrap_or(false) {
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

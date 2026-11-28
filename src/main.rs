#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod applications;
mod audio;
mod control_panel;
mod ffmpeg;
mod localization;
mod overlay;
mod path_picker;
mod recorder;
mod settings;
mod startup;
mod tray_panel;
mod updater;
mod window_events;

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Sender},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use control_panel::{ControlPanel, QuickSettings};
use localization::Locale;
use overlay::{Notice, Overlay};
use recorder::{Display, Recorder, SaveOutcome, recording_error_text};
use serde::{Deserialize, Serialize};
use serde_json::json;
use settings::{CloseBehavior, OverlayShortcut, Preferences, Settings};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::{Color, NewWindowResponse};
use tauri::{
    PhysicalPosition, RunEvent, State, Theme, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    WindowEvent,
};
use tray_panel::TrayPanel;
use windows::Win32::Foundation::{ERROR_CANCELLED, ERROR_SHARING_VIOLATION, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_F1,
};
use windows::Win32::UI::Shell::{
    IQueryContinue, IUserNotification2, IUserNotificationCallback, NIIF_ERROR, NIIF_INFO,
    NIIF_NOSOUND, SetCurrentProcessExplicitAppUserModelID, ShellExecuteW, UserNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW, SW_SHOWNORMAL};
use windows::core::{HSTRING, PCWSTR, w};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    Ready,
    Start,
    FfmpegChoice {
        #[serde(flatten)]
        choice: FfmpegChoice,
    },
    Stop,
    Save,
    Apply {
        settings: Box<Settings>,
    },
    RefreshDevices,
    RefreshApplications,
    CheckUpdate,
    DownloadUpdate,
    InstallUpdate,
    OpenFolder,
    OpenClip,
    OpenOverlay,
    OverlayShortcut {
        shortcut: OverlayShortcut,
    },
    CaptureOverlayShortcut {
        enabled: bool,
    },
    OpenRepository,
    Preferences {
        preferences: Preferences,
    },
    CloseChoice {
        behavior: CloseBehavior,
        remember: bool,
    },
}

#[derive(Deserialize)]
#[serde(tag = "choice", rename_all = "snake_case")]
enum FfmpegChoice {
    Download,
    Path { path: PathBuf },
    Exit,
    Cancel,
}

enum AppEvent {
    Tick,
    CloseMain,
    Request(Request),
    Hotkey,
    Devices(Result<[Vec<audio::Device>; 2]>),
    UpdateChecked(Result<Option<updater::Update>>),
    UpdateDownloaded(Result<updater::Download>),
    FfmpegInstalled(Result<PathBuf>),
    FfmpegProgress(ffmpeg::Progress),
    TrayMenu(PhysicalPosition<f64>),
    TrayReady,
    ToggleBuffer,
    DismissTray,
    ControlPanel,
    OverlayHotkey,
    ControlReady,
    DismissControl,
    ControlDimmingCleared(u32),
    QuickSettings(QuickSettings),
    Show,
    Quit,
}

#[derive(Serialize)]
struct UiStatus<'a> {
    #[serde(flatten)]
    capture: &'a recorder::Status<'a>,
    download: Option<ffmpeg::Progress>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ControlAction {
    Ready,
    Dismiss,
    DimmingCleared { close_id: u32 },
    ToggleBuffer,
    Save,
    Apply { settings: QuickSettings },
}

fn ipc(view: &WebviewWindow, name: &str, value: &impl serde::Serialize) {
    if let Ok(json) = serde_json::to_string(value) {
        let _ = view.eval(format!("window.rewind?.{name}({json});"));
    }
}

#[tauri::command]
fn dispatch(
    window: WebviewWindow,
    events: State<'_, Sender<AppEvent>>,
    request: Request,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("Команда доступна в главном окне.".into());
    }
    events
        .send(AppEvent::Request(request))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn tray_action(
    window: WebviewWindow,
    events: State<'_, Sender<AppEvent>>,
    action: String,
) -> Result<(), String> {
    if window.label() != "tray" {
        return Err("Команда доступна в меню Rewind.".into());
    }
    let event = match action.as_str() {
        "ready" => AppEvent::TrayReady,
        "dismiss" => AppEvent::DismissTray,
        "open" => AppEvent::Show,
        "overlay" => AppEvent::ControlPanel,
        "save" => AppEvent::Hotkey,
        "toggle_buffer" => AppEvent::ToggleBuffer,
        "quit" => AppEvent::Quit,
        _ => return Err("Неизвестная команда меню.".into()),
    };
    events.send(event).map_err(|error| error.to_string())
}

#[tauri::command]
fn control_action(
    window: WebviewWindow,
    events: State<'_, Sender<AppEvent>>,
    request: ControlAction,
) -> Result<(), String> {
    if window.label() != control_panel::LABEL {
        return Err("Команда доступна в оверлее Rewind.".into());
    }
    let event = match request {
        ControlAction::Ready => AppEvent::ControlReady,
        ControlAction::Dismiss => AppEvent::DismissControl,
        ControlAction::DimmingCleared { close_id } => AppEvent::ControlDimmingCleared(close_id),
        ControlAction::ToggleBuffer => AppEvent::ToggleBuffer,
        ControlAction::Save => AppEvent::Request(Request::Save),
        ControlAction::Apply { settings } => AppEvent::QuickSettings(settings),
    };
    events.send(event).map_err(|error| error.to_string())
}

fn audio_devices(proxy: &Sender<AppEvent>) {
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let _ = proxy.send(AppEvent::Devices(audio::devices()));
    });
}

fn monitors() -> Vec<Display> {
    unsafe extern "system" fn visit(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        // EnumDisplayMonitors calls synchronously while the vector is borrowed.
        let displays = unsafe { &mut *(data.0 as *mut Vec<Display>) };
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            let rect = info.monitorInfo.rcMonitor;
            let primary = info.monitorInfo.dwFlags & 1 != 0;
            let raw = String::from_utf16_lossy(&info.szDevice);
            let name = raw.trim_matches(char::from(0)).trim_start_matches(r"\\.\");
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            displays.push(Display {
                label: format!("{name} · {width} × {height}"),
                primary,
                x: rect.left,
                handle: monitor.0 as usize,
                y: rect.top,
                width,
                height,
            });
        }
        true.into()
    }
    let mut list: Vec<Display> = Vec::new();
    let _ =
        unsafe { EnumDisplayMonitors(None, None, Some(visit), LPARAM((&raw mut list) as isize)) };
    list.sort_by_key(|display| (!display.primary, display.x, display.y));
    list
}

fn register_shortcut(window: HWND, shortcut: u32) -> bool {
    unsafe {
        RegisterHotKey(
            Some(window),
            shortcut as i32,
            MOD_ALT | MOD_NOREPEAT,
            u32::from(VK_F1.0) + shortcut - 1,
        )
    }
    .is_ok()
}

fn unregister_shortcut(window: HWND, shortcut: u32) {
    let _ = unsafe { UnregisterHotKey(Some(window), shortcut as i32) };
}

fn last_clip(directory: &Path) -> Option<PathBuf> {
    fs::read_dir(directory)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let metadata = entry.metadata().ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            (metadata.is_file() && name.starts_with("Replay-") && name.ends_with(".mp4"))
                .then(|| (metadata.modified().ok(), entry.path()))
        })
        .max()
        .map(|(_, path)| path)
}

fn notify(overlay: &mut Option<Overlay>, locale: Locale, notice: Notice, message: &str) {
    let message = locale.text(message);
    if let Some(overlay) = overlay
        && overlay.show(notice, locale, &message).is_ok()
    {
        return;
    }
    let title = format!("Rewind · {}", locale.text(notice.title()));
    let icon = if notice.is_error() {
        NIIF_ERROR
    } else {
        NIIF_INFO
    };
    let message: String = message
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect();
    let _ = std::thread::Builder::new()
        .name("rewind-notification".into())
        .spawn(move || {
            if let Err(error) = show_notification(&title, &message, (icon | NIIF_NOSOUND).0) {
                eprintln!("Не удалось показать уведомление: {error}");
            }
        });
}

fn show_notification(title: &str, message: &str, flags: u32) -> windows::core::Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let result = (|| {
            let notice: IUserNotification2 =
                CoCreateInstance(&UserNotification, None, CLSCTX_INPROC_SERVER)?;
            notice.SetBalloonInfo(&HSTRING::from(title), &HSTRING::from(message), flags)?;
            notice.SetBalloonRetry(5000, 0, 0)?;
            match notice.Show(
                None::<&IQueryContinue>,
                0,
                None::<&IUserNotificationCallback>,
            ) {
                // The shell also reports cancellation when an unclicked balloon expires.
                Err(error) if error.code() == ERROR_CANCELLED.to_hresult() => Ok(()),
                result => result,
            }
        })();
        CoUninitialize();
        result
    }
}

fn launch(target: &OsStr) -> Result<()> {
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR::null(),
            &HSTRING::from(target),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    ensure!(
        result.0 as isize > 32,
        "Не удалось открыть {}",
        target.to_string_lossy()
    );
    Ok(())
}

fn show(window: &WebviewWindow) {
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

fn close(window: &WebviewWindow, behavior: CloseBehavior, quitting: &mut bool) -> Result<()> {
    match behavior {
        CloseBehavior::Tray => window.hide()?,
        CloseBehavior::Exit => *quitting = true,
        CloseBehavior::Ask => {}
    }
    Ok(())
}

fn save_preferences(directory: &Path, settings: &mut Settings, next: Preferences) -> Result<()> {
    anyhow::ensure!(
        [80, 90, 100, 110, 125, 150].contains(&next.interface_scale),
        "Недопустимый масштаб интерфейса"
    );
    let previous = settings.preferences;
    let startup_changed = previous.start_with_windows != next.start_with_windows;
    if startup_changed {
        startup::set_enabled(next.start_with_windows)?;
    }
    settings.preferences = next;
    if let Err(error) = settings.write(directory) {
        settings.preferences = previous;
        if startup_changed {
            startup::set_enabled(previous.start_with_windows)?;
        }
        return Err(error).context("Не удалось сохранить настройки");
    }
    Ok(())
}

fn open_data() -> Result<(PathBuf, File)> {
    let directory = std::env::var_os("REWIND_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_local_dir().map(|path| path.join("Rewind")))
        .context("Не удалось найти папку данных приложения")?;
    fs::create_dir_all(&directory)?;
    let directory = directory.canonicalize()?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(directory.join("rewind.lock"))
        .context("Rewind уже открыт или папка данных недоступна")?;
    Ok((directory, lock))
}

fn open_buffers(directory: &Path) -> Result<PathBuf> {
    let buffers = directory.join("buffers");
    fs::create_dir_all(&buffers)?;
    for entry in fs::read_dir(&buffers)?.flatten() {
        if entry.file_name().to_string_lossy().starts_with("buffer-")
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(buffers)
}

fn run() -> Result<()> {
    let windows_startup = std::env::args_os().any(|arg| arg == startup::ARGUMENT);
    let Some(_instance) = window_events::single_instance(!windows_startup)? else {
        return Ok(());
    };
    let (directory, lock) = match open_data() {
        Err(error)
            if windows_startup
                && error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    error.raw_os_error() == Some(ERROR_SHARING_VIOLATION.0 as i32)
                }) =>
        {
            return Ok(());
        }
        result => result?,
    };
    let (mut settings, load_error) = match Settings::load(&directory).and_then(|settings| {
        settings.validate()?;
        Ok(settings)
    }) {
        Ok(settings) => (settings, None),
        Err(error) => (
            Settings::default(),
            Some(format!("{error:#}. Проверьте настройки перед записью.")),
        ),
    };
    if windows_startup && !settings.preferences.start_with_windows {
        return Ok(());
    }
    let buffers = open_buffers(&directory)?;
    let mut recorder = Recorder::default();
    recorder.last = last_clip(&settings.output);
    recorder.error = load_error;
    let mut displays = monitors();
    let (proxy, incoming) = mpsc::channel();
    let app = tauri::Builder::default()
        .manage(proxy.clone())
        .invoke_handler(tauri::generate_handler![
            dispatch,
            tray_action,
            control_action,
            path_picker::browse_path
        ])
        .build(tauri::generate_context!())?;
    let webview_data = directory.join("webview");
    let view = WebviewWindowBuilder::new(&app, "main", WebviewUrl::App("index.html".into()))
        .title("Rewind")
        .theme(Some(Theme::Dark))
        .inner_size(1180.0, 800.0)
        .min_inner_size(560.0, 600.0)
        .decorations(false)
        .background_color(Color(0, 0, 0, 255))
        .data_directory(webview_data.clone())
        .on_navigation(|url| {
            url.host_str() == Some("tauri.localhost") && matches!(url.path(), "/" | "/index.html")
        })
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build()
        .context(
            "Не удалось открыть интерфейс Rewind. Проверьте, установлен ли Microsoft Edge WebView2",
        )?;
    let hwnd = view.hwnd()?;
    let window_events = window_events::WindowEvents::attach(hwnd, proxy.clone())?;
    let mut shortcut = register_shortcut(hwnd, settings.shortcut);
    let mut overlay_hotkey = control_panel::register(
        hwnd,
        control_panel::HOTKEY,
        settings.preferences.overlay_shortcut,
    )
    .then_some(control_panel::HOTKEY);
    let mut tray_panel = TrayPanel::new(&app, webview_data.clone())
        .inspect_err(|error| eprintln!("Не удалось создать меню Rewind: {error:#}"))
        .ok();
    let custom_tray = tray_panel.is_some();
    let tray_proxy = proxy.clone();
    let skip_left_release = AtomicBool::new(false);
    let mut tray_builder = TrayIconBuilder::with_id("rewind")
        .icon(
            app.default_window_icon()
                .context("Не удалось загрузить встроенную иконку Rewind")?
                .clone(),
        )
        .tooltip("Rewind")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(move |_, event| {
            let event = match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Down,
                    ..
                } => {
                    skip_left_release.store(false, Ordering::Relaxed);
                    return;
                }
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => {
                    // Windows sends one more button-up after the double-click.
                    skip_left_release.store(true, Ordering::Relaxed);
                    AppEvent::Show
                }
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    position,
                    ..
                } => {
                    if skip_left_release.swap(false, Ordering::Relaxed) {
                        return;
                    }
                    if custom_tray {
                        AppEvent::TrayMenu(position)
                    } else {
                        AppEvent::Show
                    }
                }
                TrayIconEvent::Click {
                    button: MouseButton::Right,
                    button_state: MouseButtonState::Up,
                    position,
                    ..
                } if custom_tray => AppEvent::TrayMenu(position),
                _ => return,
            };
            let _ = tray_proxy.send(event);
        });
    let mut fallback_toggle = None;
    if !custom_tray {
        let locale = settings.preferences.language.resolve();
        let open = MenuItem::with_id(
            &app,
            "open",
            locale.text("Открыть Rewind"),
            true,
            None::<&str>,
        )?;
        let save = MenuItem::with_id(
            &app,
            "save",
            locale.text("Сохранить повтор"),
            true,
            None::<&str>,
        )?;
        let quit = MenuItem::with_id(&app, "quit", locale.text("Выйти"), true, None::<&str>)?;
        let overlay_item =
            MenuItem::with_id(&app, "overlay", locale.text("Оверлей"), true, None::<&str>)?;
        let toggle = MenuItem::with_id(
            &app,
            "toggle_buffer",
            locale.text("Включить буфер"),
            true,
            None::<&str>,
        )?;
        let menu = Menu::with_items(
            &app,
            &[
                &open,
                &toggle,
                &save,
                &overlay_item,
                &PredefinedMenuItem::separator(&app)?,
                &quit,
            ],
        )?;
        fallback_toggle = Some(toggle);
        let menu_proxy = proxy.clone();
        tray_builder = tray_builder.menu(&menu).on_menu_event(move |_, event| {
            let event = match event.id().as_ref() {
                "open" => AppEvent::Show,
                "overlay" => AppEvent::ControlPanel,
                "save" => AppEvent::Hotkey,
                "toggle_buffer" => AppEvent::ToggleBuffer,
                "quit" => AppEvent::Quit,
                _ => return,
            };
            let _ = menu_proxy.send(event);
        });
    }
    let tray = tray_builder.build(&app).ok();
    let (keep_ticking, stop_ticks) = mpsc::channel::<()>();
    let tick_proxy = proxy.clone();
    let tick_handle = app.handle().clone();
    std::thread::Builder::new()
        .name("rewind-clock".into())
        .spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) =
                stop_ticks.recv_timeout(Duration::from_millis(180))
            {
                // Wake the Tauri loop even when both WebViews are hidden.
                if tick_proxy.send(AppEvent::Tick).is_err()
                    || tick_handle.run_on_main_thread(|| {}).is_err()
                {
                    break;
                }
            }
        })?;
    let mut ready = false;
    let mut close_pending = false;
    let mut quitting = false;
    let mut overlay = Overlay::new().ok();
    let mut control: Option<ControlPanel> = None;
    let mut awaiting_ffmpeg = false;
    let mut downloading: Option<Arc<AtomicBool>> = None;
    let mut ffmpeg_progress = None;
    let mut start_on_ready = windows_startup && settings.resume_buffer;
    let mut buffer_active = false;
    let mut previous_phase = "idle";
    let mut capturing_overlay_shortcut = false;
    let mut update_status = "idle";
    let mut available_update: Option<updater::Update> = None;
    let mut staged_update: Option<updater::Download> = None;
    let mut next_update_check = Instant::now();
    let mut fallback_state = None;
    app.run_return(move |app, event| {
        let _ = (&lock, &window_events, &keep_ticking);
        let start_capture = |recorder: &mut Recorder, displays: &mut Vec<Display>, settings: &Settings| {
            *displays = monitors();
            recorder.start(&buffers, settings, displays, false)
        };
        if let RunEvent::WindowEvent { label, event: WindowEvent::CloseRequested { api, .. }, .. } = event {
            api.prevent_close();
            let event = match label.as_str() {
                "tray" => AppEvent::DismissTray,
                control_panel::LABEL => AppEvent::DismissControl,
                _ => AppEvent::CloseMain,
            };
            let _ = proxy.send(event);
        }
        for event in incoming.try_iter() {
            let locale = settings.preferences.language.resolve();
            let send_preferences = |settings: &Settings| {
                let _ = view.set_zoom(f64::from(settings.preferences.interface_scale) / 100.0);
                ipc(&view, "preferences", &json!({
                    "preferences": settings.preferences,
                    "locale": settings.preferences.language.resolve(),
                    "tray": tray.is_some(),
                    "version": env!("CARGO_PKG_VERSION")
                }));
            };
            let send_settings = |settings: &Settings, displays: &[Display], shortcut: bool| {
                let detected_ffmpeg = ffmpeg::find(&settings.ffmpeg, &directory);
                ipc(&view, "configure", &json!({"settings": settings, "displays": displays, "shortcut": shortcut, "tray": tray.is_some(), "detected_ffmpeg": detected_ffmpeg}));
            };
            if matches!(&event, AppEvent::Show | AppEvent::Hotkey | AppEvent::Quit | AppEvent::ControlPanel | AppEvent::OverlayHotkey)
                && let Some(panel) = &mut tray_panel
            {
                panel.hide();
            }
            let due = matches!(event, AppEvent::Tick);
            let new_frontend = matches!(event, AppEvent::Request(Request::Ready));
            let update_before = update_status;
            let capture_action = matches!(
                &event,
                AppEvent::Request(Request::Start | Request::Stop | Request::FfmpegChoice { choice: FfmpegChoice::Download | FfmpegChoice::Path { .. } })
                    | AppEvent::FfmpegInstalled(_)
            );
            let result = match event {
                AppEvent::TrayMenu(position) => {
                    if let Some(panel) = &mut tray_panel {
                        panel.open(position).context("Не удалось открыть меню Rewind")
                    } else {
                        Ok(())
                    }
                }
                AppEvent::TrayReady => {
                    if let Some(panel) = &mut tray_panel {
                        panel.reveal();
                    }
                    Ok(())
                }
                AppEvent::DismissTray => {
                    if let Some(panel) = &mut tray_panel {
                        panel.hide();
                    }
                    Ok(())
                }
                AppEvent::OverlayHotkey if capturing_overlay_shortcut => {
                    capturing_overlay_shortcut = false;
                    ipc(&view, "overlayShortcut", &json!({ "shortcut": settings.preferences.overlay_shortcut, "registered": overlay_hotkey.is_some() }));
                    Ok(())
                }
                AppEvent::Hotkey if capturing_overlay_shortcut => {
                    capturing_overlay_shortcut = false;
                    ipc(&view, "overlayShortcut", &json!({ "shortcut": settings.preferences.overlay_shortcut, "registered": overlay_hotkey.is_some(), "error": locale.text("Это сочетание уже сохраняет повтор.") }));
                    Ok(())
                }
                AppEvent::ControlPanel | AppEvent::OverlayHotkey => (|| -> Result<()> {
                    if !quitting {
                        if control.is_none() {
                            control = Some(ControlPanel::new(app, webview_data.clone())?);
                        }
                        if let Some(panel) = &mut control {
                            panel.toggle()?;
                        }
                    }
                    Ok(())
                })(),
                AppEvent::ControlReady => {
                    if let Some(panel) = &mut control { panel.refresh(); }
                    Ok(())
                }
                AppEvent::DismissControl => {
                    if let Some(panel) = &mut control { panel.hide(); }
                    Ok(())
                }
                AppEvent::ControlDimmingCleared(close_id) => {
                    if let Some(panel) = &mut control { panel.finish_hide(close_id); }
                    Ok(())
                }
                AppEvent::QuickSettings(quick) => {
                    let update = (|| -> Result<()> {
                        ensure!(!quitting && downloading.is_none() && !awaiting_ffmpeg && recorder.can_configure(), "Остановите запись и дождитесь завершения текущей операции.");
                        let mut next = settings.clone();
                        next.seconds = quick.seconds;
                        next.fps = quick.fps;
                        next.export_codec = quick.export_codec;
                        next.validate()?;
                        next.write(&directory).context("Не удалось сохранить настройки")?;
                        settings = next;
                        recorder.error = None;
                        ipc(&view, "quickSettings", &quick);
                        Ok(())
                    })();
                    if let Some(panel) = &control {
                        let error = update.as_ref().err().map(|error| format!("{error:#}"));
                        let message = error.as_deref().map(|message| locale.text(message));
                        panel.applied(message.as_deref());
                    }
                    update
                }
                AppEvent::ToggleBuffer => {
                    let status = recorder.status();
                    let phase = if downloading.is_some() { "downloading" } else { status.phase };
                    if !quitting && tray_panel::buffer_control(phase, status.saving).1 {
                        if awaiting_ffmpeg {
                            if let Some(panel) = &mut control { panel.hide(); }
                            show(&view);
                        } else {
                            let request = if phase == "idle" { Request::Start } else { Request::Stop };
                            let _ = proxy.send(AppEvent::Request(request));
                        }
                    }
                    Ok(())
                }
                AppEvent::CloseMain => {
                    let behavior = settings.preferences.close_behavior;
                    if behavior == CloseBehavior::Ask || (behavior == CloseBehavior::Tray && tray.is_none()) {
                        if ready {
                            ipc(&view, "closePrompt", &tray.is_some());
                        } else {
                            close_pending = true;
                        }
                        Ok(())
                    } else {
                        close(&view, behavior, &mut quitting)
                    }
                }
                AppEvent::Quit => {
                    quitting = true;
                    Ok(())
                }
                AppEvent::Show => {
                    if let Some(panel) = &mut control { panel.hide(); }
                    show(&view);
                    Ok(())
                }
                AppEvent::Devices(devices) => {
                    match devices {
                        Ok(devices) => ipc(&view, "devices", &devices),
                        Err(error) => ipc(&view, "deviceError", &locale.text(&format!("{error:#}"))),
                    }
                    Ok(())
                }
                AppEvent::UpdateChecked(result) => {
                    update_status = match result {
                        Ok(Some(update)) => {
                            available_update = Some(update);
                            "available"
                        }
                        Ok(None) => "current",
                        Err(_) => "check_error",
                    };
                    Ok(())
                }
                AppEvent::UpdateDownloaded(result) => {
                    update_status = match result {
                        Ok(update) => {
                            staged_update = Some(update);
                            "downloaded"
                        }
                        Err(_) => "download_error",
                    };
                    Ok(())
                }
                AppEvent::FfmpegProgress(progress) => {
                    if downloading.is_some() { ffmpeg_progress = Some(progress); }
                    Ok(())
                }
                AppEvent::FfmpegInstalled(result) => (|| -> Result<()> {
                    ffmpeg_progress = None;
                    let cancelled = downloading.take().is_some_and(|stop| stop.load(Ordering::Relaxed));
                    if !quitting && !cancelled {
                        let path = result?;
                        ipc(&view, "detectedFfmpeg", &path);
                        let mut capture = settings.clone();
                        capture.ffmpeg = path;
                        start_capture(&mut recorder, &mut displays, &capture)?;
                    }
                    Ok(())
                })(),
                AppEvent::Hotkey | AppEvent::Request(Request::Save) => {
                    let result = if downloading.is_some() {
                        Err(anyhow::anyhow!("Дождитесь установки FFmpeg."))
                    } else {
                        recorder.save(settings.export_codec)
                    };
                    match &result {
                        Ok(true) => notify(&mut overlay, locale, Notice::Saving, &format!("Rewind · MP4 / {}", settings.export_codec.label())),
                        Ok(false) => {}
                        Err(error) => notify(&mut overlay, locale, Notice::Failed, &error.to_string()),
                    }
                    result.map(|_| ())
                }
                AppEvent::Request(request) => (|| -> Result<()> {
                    match request {
                        Request::Ready => {
                            ready = true;
                            ipc(&view, "overlayShortcut", &json!({"shortcut": settings.preferences.overlay_shortcut, "registered": overlay_hotkey.is_some()}));
                            send_preferences(&settings);
                            send_settings(&settings, &displays, shortcut);
                            audio_devices(&proxy);
                            ipc(&view, "applications", &applications::running());
                            if awaiting_ffmpeg {
                                ipc(&view, "ffmpegPrompt", &true);
                            }
                            if std::mem::take(&mut close_pending) {
                                ipc(&view, "closePrompt", &tray.is_some());
                            }
                            if std::mem::take(&mut start_on_ready) {
                                let _ = proxy.send(AppEvent::Request(Request::Start));
                            }
                        }
                        Request::Preferences { mut preferences } => {
                            preferences.overlay_shortcut = settings.preferences.overlay_shortcut;
                            let enable_updates = preferences.auto_update && !settings.preferences.auto_update;
                            let result = save_preferences(&directory, &mut settings, preferences);
                            send_preferences(&settings);
                            if result.is_err() {
                                ipc(&view, "preferencesSaveError", &true);
                            }
                            result?;
                            if enable_updates {
                                next_update_check = Instant::now();
                            }
                            ipc(&view, "preferencesSaved", &true);
                        }
                        Request::CloseChoice { behavior, remember } => {
                            ensure!(behavior != CloseBehavior::Ask, "Выберите, как закрыть Rewind.");
                            ensure!(behavior != CloseBehavior::Tray || tray.is_some(), "Область уведомлений Windows недоступна.");
                            if remember {
                                let preferences = Preferences { close_behavior: behavior, ..settings.preferences };
                                save_preferences(&directory, &mut settings, preferences)?;
                                send_preferences(&settings);
                            }
                            close(&view, behavior, &mut quitting)?;
                        }
                        Request::Start => {
                            if downloading.is_none() && !awaiting_ffmpeg && !quitting {
                                ensure!(recorder.can_configure(), "Сначала остановите текущую запись.");
                                let detected = ffmpeg::find(&settings.ffmpeg, &directory);
                                ipc(&view, "detectedFfmpeg", &detected);
                                if let Some(path) = detected {
                                    let mut capture = settings.clone();
                                    capture.ffmpeg = path;
                                    start_capture(&mut recorder, &mut displays, &capture)?;
                                } else {
                                    recorder.error = None;
                                    awaiting_ffmpeg = true;
                                    if let Some(panel) = &mut control { panel.hide(); }
                                    show(&view);
                                    ipc(&view, "ffmpegPrompt", &true);
                                }
                            }
                        }
                        Request::FfmpegChoice { choice } => {
                            if std::mem::take(&mut awaiting_ffmpeg) && !quitting {
                                match choice {
                                    FfmpegChoice::Cancel => {}
                                    FfmpegChoice::Exit => quitting = true,
                                    FfmpegChoice::Download => {
                                        ensure!(recorder.can_configure(), "Сначала дождитесь остановки записи или сохранения клипа.");
                                        let stop = Arc::new(AtomicBool::new(false));
                                        downloading = Some(Arc::clone(&stop));
                                        ffmpeg_progress = Some(ffmpeg::Progress::Connecting { mirror: false });
                                        let proxy = proxy.clone();
                                        let directory = directory.clone();
                                        std::thread::spawn(move || {
                                            let result = ffmpeg::install(&directory, &stop, |progress| {
                                                let _ = proxy.send(AppEvent::FfmpegProgress(progress));
                                            });
                                            let _ = proxy.send(AppEvent::FfmpegInstalled(result));
                                        });
                                    }
                                    FfmpegChoice::Path { path } => {
                                        ensure!(recorder.can_configure(), "Сначала дождитесь остановки записи или сохранения клипа.");
                                        ensure!(path.is_absolute() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) && path.is_file(), "Укажите существующий файл ffmpeg.exe.");
                                        let mut next = settings.clone();
                                        next.ffmpeg = path;
                                        next.write(&directory).context("Не удалось сохранить настройки")?;
                                        settings.ffmpeg = next.ffmpeg;
                                        ipc(&view, "ffmpegPath", &settings.ffmpeg);
                                        start_capture(&mut recorder, &mut displays, &settings)?;
                                    }
                                }
                            }
                        }
                        Request::Stop => {
                            if let Some(stop) = &downloading {
                                stop.store(true, Ordering::Relaxed);
                            } else {
                                recorder.stop()?;
                            }
                        }
                        Request::Apply { settings: mut next } => {
                            ensure!(downloading.is_none() && recorder.can_configure(), "Остановите запись и дождитесь завершения текущей операции.");
                            next.preferences = settings.preferences;
                            next.resume_buffer = settings.resume_buffer;
                            next.validate()?;
                            let changed = next.shortcut != settings.shortcut;
                            let newly_registered = (changed || !shortcut) && register_shortcut(hwnd, next.shortcut);
                            if let Err(error) = next.write(&directory) {
                                if newly_registered { unregister_shortcut(hwnd, next.shortcut); }
                                return Err(error).context("Не удалось сохранить настройки");
                            }
                            if changed && shortcut { unregister_shortcut(hwnd, settings.shortcut); }
                            shortcut = newly_registered || (!changed && shortcut);
                            if settings.output != next.output {
                                recorder.last = last_clip(&next.output);
                            }
                            settings = *next;
                            recorder.error = None;
                            send_settings(&settings, &displays, shortcut);
                            ipc(&view, "applied", &true);
                        }
                        Request::RefreshDevices => audio_devices(&proxy),
                        Request::RefreshApplications => ipc(&view, "applications", &applications::running()),
                        Request::CheckUpdate => {
                            if !matches!(update_status, "checking" | "downloading" | "downloaded") {
                                update_status = "checking";
                                available_update = None;
                                next_update_check = Instant::now() + Duration::from_secs(12 * 60 * 60);
                                let proxy = proxy.clone();
                                std::thread::spawn(move || {
                                    let _ = proxy.send(AppEvent::UpdateChecked(updater::check()));
                                });
                            }
                        }
                        Request::DownloadUpdate => {
                            if matches!(update_status, "available" | "download_error")
                                && let Some(update) = &available_update
                            {
                                update_status = "downloading";
                                let update = update.clone();
                                let proxy = proxy.clone();
                                std::thread::spawn(move || {
                                    let _ = proxy.send(AppEvent::UpdateDownloaded(updater::download(&update)));
                                });
                            }
                        }
                        Request::InstallUpdate => {
                            if recorder.can_configure() && downloading.is_none() && !awaiting_ffmpeg
                                && let Some(update) = staged_update.take()
                            {
                                if update.install().is_ok() {
                                    quitting = true;
                                } else {
                                    update_status = "download_error";
                                }
                            }
                        }
                        Request::OpenFolder => {
                            fs::create_dir_all(&settings.output)?;
                            launch(settings.output.as_os_str())?;
                        }
                        Request::OpenClip => launch(recorder.last.as_deref().context("Сначала сохраните клип.")?.as_os_str())?,
                        Request::OpenOverlay => { let _ = proxy.send(AppEvent::ControlPanel); }
                        Request::CaptureOverlayShortcut { enabled } => capturing_overlay_shortcut = enabled,
                        Request::OverlayShortcut { shortcut } => {
                            capturing_overlay_shortcut = false;
                            let previous = settings.preferences.overlay_shortcut;
                            let update = (|| -> Result<()> {
                                ensure!(shortcut.valid(), "Выберите сочетание с Alt, Ctrl, Shift или Win.");
                                if previous == shortcut && overlay_hotkey.is_some() { return Ok(()); }
                                let id = if overlay_hotkey == Some(control_panel::HOTKEY) { control_panel::HOTKEY_ALT } else { control_panel::HOTKEY };
                                ensure!(control_panel::register(hwnd, id, shortcut), "Это сочетание занято или зарезервировано Windows.");
                                let mut preferences = settings.preferences;
                                preferences.overlay_shortcut = shortcut;
                                if let Err(error) = save_preferences(&directory, &mut settings, preferences) {
                                    let _ = unsafe { UnregisterHotKey(Some(hwnd), id) };
                                    return Err(error);
                                }
                                if let Some(old) = overlay_hotkey.replace(id) {
                                    let _ = unsafe { UnregisterHotKey(Some(hwnd), old) };
                                }
                                Ok(())
                            })();
                            let error = update.err().map(|error| locale.text(&format!("{error:#}")).into_owned());
                            ipc(&view, "overlayShortcut", &json!({"shortcut": settings.preferences.overlay_shortcut, "registered": overlay_hotkey.is_some(), "error": error}));
                        }
                        Request::OpenRepository => launch(OsStr::new(env!("CARGO_PKG_REPOSITORY")))?,
                        Request::Save => {}
                    }
                    Ok(())
                })(),
                AppEvent::Tick => Ok(()),
            };
            let mut capture_failed = false;
            if due {
                if ready && !quitting && settings.preferences.auto_update
                    && !matches!(update_status, "checking" | "downloading" | "downloaded")
                    && Instant::now() >= next_update_check
                {
                    next_update_check = Instant::now() + Duration::from_secs(12 * 60 * 60);
                    let _ = proxy.send(AppEvent::Request(Request::CheckUpdate));
                }
                if let Some(overlay) = &mut overlay {
                    overlay.tick(Instant::now());
                }
                let mut capture_error = None;
                let saved = recorder.tick(|message| capture_error = Some(message.to_owned()));
                capture_failed = capture_error.is_some();
                match (capture_error, saved) {
                    (Some(message), _) => notify(&mut overlay, locale, Notice::RecordingFailed, &message),
                    (None, Some(SaveOutcome::Saved)) => {
                        let name = recorder.last.as_deref().and_then(Path::file_name).and_then(|name| name.to_str());
                        notify(&mut overlay, locale, Notice::Saved, name.unwrap_or("Клип появился в папке сохранения."));
                    }
                    (None, Some(SaveOutcome::Failed)) => notify(&mut overlay, locale, Notice::Failed, recorder.error.as_deref().unwrap_or("Ошибка сохранения файла.")),
                    (None, None) => {}
                }
            }
            if let Err(error) = result {
                let message = if capture_action {
                    let message = recording_error_text(&error);
                    notify(&mut overlay, locale, Notice::RecordingFailed, &message);
                    message
                } else {
                    format!("{error:#}")
                };
                recorder.error = Some(message);
                ipc(&view, "rejected", &true);
            }
            let mut status = recorder.status();
            if !quitting && !capture_failed {
                match (previous_phase, status.phase) {
                    ("starting", "recording") => notify(&mut overlay, locale, Notice::BufferStarted, "Идёт запись"),
                    ("stopping", "idle") => notify(&mut overlay, locale, Notice::BufferStopped, if status.seconds > 0.0 { "Накопленный повтор можно сохранить." } else { "Запись остановлена" }),
                    _ => {}
                }
            }
            previous_phase = status.phase;
            let active = matches!(status.phase, "starting" | "recording");
            if active != buffer_active {
                buffer_active = active;
                settings.resume_buffer = active;
                if let Err(error) = settings.write(&directory) {
                    recorder.error = Some(format!("Не удалось запомнить состояние буфера: {error:#}"));
                    status = recorder.status();
                }
            }
            if let Some(stop) = &downloading {
                if quitting { stop.store(true, Ordering::Relaxed); }
                status.phase = "downloading";
            }
            let locale = settings.preferences.language.resolve();
            let local_error = status.error.map(|error| locale.text(error));
            status.error = local_error.as_deref();
            if ready {
                ipc(&view, "status", &UiStatus { capture: &status, download: ffmpeg_progress });
                if new_frontend || update_status != update_before {
                    ipc(&view, "updateState", &json!({
                        "state": update_status,
                        "version": available_update.as_ref().map(|update| &update.version),
                    }));
                }
            }
            if let Some(panel) = &mut tray_panel {
                panel.update(&status, status.limit.unwrap_or(settings.seconds), shortcut.then_some(settings.shortcut), locale, ffmpeg_progress);
            }
            if let Some(panel) = &mut control {
                panel.update(&status, &settings, locale, ffmpeg_progress);
            }
            if let Some(toggle) = &fallback_toggle {
                let (label, enabled) = tray_panel::buffer_control(status.phase, status.saving);
                if fallback_state != Some((label, enabled, locale)) {
                    let _ = toggle.set_text(locale.text(label));
                    let _ = toggle.set_enabled(enabled);
                    fallback_state = Some((label, enabled, locale));
                }
            }
            if quitting && !status.saving && downloading.is_none() {
                unregister_shortcut(hwnd, settings.shortcut);
                if let Some(id) = overlay_hotkey { let _ = unsafe { UnregisterHotKey(Some(hwnd), id) }; }
                app.exit(0);
                break;
            }
        }
    });
    Ok(())
}

fn main() {
    let _ = unsafe { SetCurrentProcessExplicitAppUserModelID(w!("Rewind.Replay")) };
    if let Err(error) = run() {
        let message = format!("{error:#}");
        let message = Locale::system().text(&message);
        unsafe {
            MessageBoxW(
                Some(HWND::default()),
                &HSTRING::from(message.as_ref()),
                &HSTRING::from("Rewind"),
                MB_ICONERROR,
            )
        };
    }
}

#[cfg(test)]
#[test]
#[ignore = "shows a test notification on the Windows desktop"]
fn windows_accepts_a_replay_notification() -> windows::core::Result<()> {
    show_notification(
        "Rewind · Тест уведомлений",
        "Проверка уведомлений о сохранении повтора.",
        (NIIF_INFO | NIIF_NOSOUND).0,
    )
}

use std::cell::RefCell;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::OnceLock;

use anyhow::{Result, ensure};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::PtInRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetWindowRect, HC_ACTION, HHOOK, HWND_BROADCAST, IsWindowVisible,
    MSLLHOOKSTRUCT, PostMessageW, RegisterWindowMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_MOUSE_LL, WM_CLOSE, WM_HOTKEY, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN,
    WM_XBUTTONDOWN,
};
use windows::core::{PCWSTR, w};

use crate::AppEvent;

#[derive(Default)]
struct OutsideWindows {
    hook: Option<HHOOK>,
    windows: Vec<HWND>,
}

thread_local! {
    static OUTSIDE_WINDOWS: RefCell<OutsideWindows> = RefCell::new(OutsideWindows::default());
}

pub struct DismissOutside(HWND);

impl DismissOutside {
    pub fn watch(hwnd: HWND) -> windows::core::Result<Self> {
        OUTSIDE_WINDOWS.with_borrow_mut(|state| {
            if state.hook.is_none() {
                state.hook = Some(unsafe {
                    let module = GetModuleHandleW(PCWSTR::null())?;
                    SetWindowsHookExW(
                        WH_MOUSE_LL,
                        Some(outside_click),
                        Some(HINSTANCE(module.0)),
                        0,
                    )?
                });
            }
            state.windows.push(hwnd);
            Ok(Self(hwnd))
        })
    }
}

impl Drop for DismissOutside {
    fn drop(&mut self) {
        OUTSIDE_WINDOWS.with_borrow_mut(|state| {
            state.windows.retain(|&hwnd| hwnd != self.0);
            if state.windows.is_empty()
                && let Some(hook) = state.hook.take()
            {
                let _ = unsafe { UnhookWindowsHookEx(hook) };
            }
        });
    }
}

unsafe extern "system" fn outside_click(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && matches!(
            wp.0 as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        )
    {
        let point = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) }.pt;
        OUTSIDE_WINDOWS.with(|shared| {
            if let Ok(state) = shared.try_borrow() {
                for &window in &state.windows {
                    let mut bounds = RECT::default();
                    unsafe {
                        if IsWindowVisible(window).as_bool()
                            && GetWindowRect(window, &mut bounds).is_ok()
                            && !PtInRect(&bounds, point).as_bool()
                        {
                            let _ = PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                    }
                }
            }
        });
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

fn show_message() -> u32 {
    static SHOW: OnceLock<u32> = OnceLock::new();
    *SHOW.get_or_init(|| unsafe { RegisterWindowMessageW(w!("Rewind.ShowMain")) })
}

pub fn single_instance(show_existing: bool) -> Result<Option<OwnedHandle>> {
    unsafe {
        let handle = CreateMutexW(None, false, w!("Local\\Rewind.Replay.SingleInstance"))?;
        let already_running = GetLastError() == ERROR_ALREADY_EXISTS;
        let handle = OwnedHandle::from_raw_handle(handle.0);
        if already_running {
            if show_existing && show_message() != 0 {
                let _ = PostMessageW(Some(HWND_BROADCAST), show_message(), WPARAM(0), LPARAM(0));
            }
            return Ok(None);
        }
        Ok(Some(handle))
    }
}

pub struct WindowEvents<F: Fn(AppEvent)> {
    window: HWND,
    _handle_event: Box<F>,
}

impl<F: Fn(AppEvent)> WindowEvents<F> {
    pub fn attach(window: HWND, handle_event: F) -> Result<Self> {
        let handle_event = Box::new(handle_event);
        ensure!(
            unsafe {
                SetWindowSubclass(
                    window,
                    Some(dispatch_message::<F>),
                    0,
                    (&*handle_event as *const F) as usize,
                )
            }
            .as_bool(),
            "Не удалось подключить события Windows."
        );
        Ok(Self {
            window,
            _handle_event: handle_event,
        })
    }
}

impl<F: Fn(AppEvent)> Drop for WindowEvents<F> {
    fn drop(&mut self) {
        unsafe {
            let _ = RemoveWindowSubclass(self.window, Some(dispatch_message::<F>), 0);
        }
    }
}

unsafe extern "system" fn dispatch_message<F: Fn(AppEvent)>(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    handle_event: usize,
) -> LRESULT {
    let event = if message == WM_HOTKEY {
        if matches!(
            wp.0 as i32,
            crate::control_panel::HOTKEY | crate::control_panel::HOTKEY_ALT
        ) {
            AppEvent::OverlayHotkey
        } else {
            AppEvent::Hotkey
        }
    } else if message != 0 && message == show_message() {
        AppEvent::Show
    } else {
        return unsafe { DefSubclassProc(window, message, wp, lp) };
    };
    // The subclass is removed before WindowEvents drops the callback.
    let handle_event = unsafe { &*(handle_event as *const F) };
    handle_event(event);
    LRESULT(0)
}

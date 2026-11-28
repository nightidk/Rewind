use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::OnceLock;
use std::sync::mpsc::Sender;

use anyhow::{Result, ensure};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, PostMessageW, RegisterWindowMessageW, WM_HOTKEY,
};
use windows::core::w;

use crate::AppEvent;

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

pub struct WindowEvents {
    window: HWND,
    _sender: Box<Sender<AppEvent>>,
}

impl WindowEvents {
    pub fn attach(window: HWND, sender: Sender<AppEvent>) -> Result<Self> {
        let sender = Box::new(sender);
        ensure!(
            unsafe {
                SetWindowSubclass(
                    window,
                    Some(dispatch_message),
                    0,
                    (&*sender as *const Sender<AppEvent>) as usize,
                )
            }
            .as_bool(),
            "Не удалось подключить события Windows."
        );
        Ok(Self {
            window,
            _sender: sender,
        })
    }
}

impl Drop for WindowEvents {
    fn drop(&mut self) {
        unsafe {
            let _ = RemoveWindowSubclass(self.window, Some(dispatch_message), 0);
        }
    }
}

unsafe extern "system" fn dispatch_message(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    _: usize,
    sender: usize,
) -> LRESULT {
    let event = if message == WM_HOTKEY {
        AppEvent::Hotkey
    } else if message != 0 && message == show_message() {
        AppEvent::Show
    } else {
        return unsafe { DefSubclassProc(window, message, wp, lp) };
    };
    // The subclass is removed before WindowEvents drops its boxed sender.
    let _ = unsafe { &*(sender as *const Sender<AppEvent>) }.send(event);
    LRESULT(0)
}

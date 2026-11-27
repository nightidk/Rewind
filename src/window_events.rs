use std::sync::mpsc::Sender;

use anyhow::{Result, ensure};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::WM_HOTKEY;

use crate::AppEvent;

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
    if message != WM_HOTKEY {
        return unsafe { DefSubclassProc(window, message, wp, lp) };
    }
    // The subclass is removed before WindowEvents drops its boxed sender.
    let _ = unsafe { &*(sender as *const Sender<AppEvent>) }.send(AppEvent::Hotkey);
    LRESULT(0)
}

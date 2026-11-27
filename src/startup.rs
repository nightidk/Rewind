use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Startup, IShellLinkW, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath, ShellLink,
};
use windows::core::{HSTRING, Interface};

pub const ARGUMENT: &str = "--windows-startup";
const SHORTCUT: &str = "Rewind.lnk";

pub fn set_enabled(enabled: bool) -> Result<()> {
    (|| {
        let folder = unsafe {
            let wide = SHGetKnownFolderPath(&FOLDERID_Startup, KF_FLAG_DONT_VERIFY, None)?;
            let folder = PathBuf::from(OsString::from_wide(wide.as_wide()));
            CoTaskMemFree(Some(wide.0.cast()));
            folder
        };
        set_shortcut(&folder, enabled)
    })()
    .context("Не удалось изменить автозапуск Rewind")
}

fn set_shortcut(folder: &Path, enabled: bool) -> Result<()> {
    let shortcut = folder.join(SHORTCUT);
    if !enabled {
        return match fs::remove_file(shortcut) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe()?;
    fs::create_dir_all(folder)?;
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let result = (|| {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(exe.as_os_str()))?;
            link.SetArguments(&HSTRING::from(ARGUMENT))?;
            link.cast::<IPersistFile>()?
                .Save(&HSTRING::from(shortcut.as_os_str()), true)
        })();
        CoUninitialize();
        result?;
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn shortcut_registers_and_removes_the_windows_startup_argument() -> Result<()> {
    use windows::Win32::System::Com::STGM_READ;

    let folder = tempfile::tempdir()?;
    let shortcut = folder.path().join(SHORTCUT);
    set_shortcut(folder.path(), true)?;
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let result = (|| -> windows::core::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.cast::<IPersistFile>()?
                .Load(&HSTRING::from(shortcut.as_os_str()), STGM_READ)?;
            let mut args = [0; 64];
            link.GetArguments(&mut args)?;
            assert_eq!(
                String::from_utf16_lossy(&args).trim_end_matches('\0'),
                ARGUMENT
            );
            Ok(())
        })();
        CoUninitialize();
        result?;
    }
    set_shortcut(folder.path(), false)?;
    assert!(!shortcut.exists());
    Ok(())
}

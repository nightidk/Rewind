use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tauri::WebviewWindow;
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IBindCtx,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog,
    IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, Result, w};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Output,
    Ffmpeg,
}

#[tauri::command]
pub async fn browse_path(
    window: WebviewWindow,
    kind: PathKind,
    current: PathBuf,
) -> std::result::Result<Option<PathBuf>, String> {
    if window.label() != "main" {
        return Err("Команда доступна в главном окне.".into());
    }
    let owner = window.hwnd().map_err(|error| error.to_string())?.0 as isize;
    tauri::async_runtime::spawn_blocking(move || unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let result = pick(HWND(owner as _), kind, &current);
        CoUninitialize();
        result
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

fn pick(owner: HWND, kind: PathKind, current: &Path) -> Result<Option<PathBuf>> {
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        let options = dialog.GetOptions()? | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
        match kind {
            PathKind::Output => dialog.SetOptions(options | FOS_PICKFOLDERS)?,
            PathKind::Ffmpeg => {
                dialog.SetOptions(options | FOS_FILEMUSTEXIST)?;
                dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
                    pszName: w!("FFmpeg (*.exe)"),
                    pszSpec: w!("*.exe"),
                }])?;
            }
        }

        if current.is_absolute()
            && let Some(folder) = current.ancestors().find(|path| path.is_dir())
            && let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                &HSTRING::from(folder.as_os_str()),
                None::<&IBindCtx>,
            )
        {
            let _ = dialog.SetFolder(&item);
        }
        match dialog.Show(Some(owner)) {
            Err(error) if error.code() == ERROR_CANCELLED.to_hresult() => return Ok(None),
            result => result?,
        }

        let wide = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        let selected = OsString::from_wide(wide.as_wide());
        CoTaskMemFree(Some(wide.0.cast()));
        Ok(Some(selected.into()))
    }
}

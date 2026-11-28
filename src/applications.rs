use std::mem::ManuallyDrop;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::{BOOL, HRESULT, Interface, PWSTR, Ref, implement};

#[derive(Serialize)]
pub struct Application {
    pub path: String,
    pub name: String,
    #[serde(skip)]
    pub pid: u32,
}

fn process(pid: u32) -> windows::core::Result<OwnedHandle> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )?;
        Ok(OwnedHandle::from_raw_handle(handle.0))
    }
}

fn process_path(process: &OwnedHandle, buffer: &mut [u16]) -> windows::core::Result<String> {
    let mut length = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            HANDLE(process.as_raw_handle()),
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )?;
    }
    Ok(String::from_utf16_lossy(&buffer[..length as usize]))
}

pub fn running() -> Vec<Application> {
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
        let (pids, apps, buffer) =
            unsafe { &mut *(data.0 as *mut (Vec<u32>, Vec<Application>, Vec<u16>)) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0
            || pid == std::process::id()
            || pids.contains(&pid)
            || unsafe { !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 }
        {
            return BOOL(1);
        }
        pids.push(pid);
        if let Ok(path) = process(pid).and_then(|process| process_path(&process, buffer))
            && !apps.iter().any(|app| app.path.eq_ignore_ascii_case(&path))
            && let Some(name) = std::path::Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
        {
            apps.push(Application {
                name: name.to_owned(),
                path,
                pid,
            });
        }
        BOOL(1)
    }

    let mut state = (
        Vec::<u32>::new(),
        Vec::<Application>::new(),
        vec![0_u16; 32_768],
    );
    let _ = unsafe { EnumWindows(Some(visit), LPARAM((&raw mut state) as isize)) };
    state.1.sort_by_key(|app| app.name.to_ascii_lowercase());
    state.1
}

pub fn matching(paths: &[String]) -> Vec<Application> {
    let mut found = Vec::<(u32, Application)>::new();
    let mut buffer = vec![0; 32_768];
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Vec::new();
        };
        let snapshot = OwnedHandle::from_raw_handle(snapshot.0);
        let handle = HANDLE(snapshot.as_raw_handle());
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut next = Process32FirstW(handle, &mut entry);
        while next.is_ok() {
            let length = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
            let pid = entry.th32ProcessID;
            if pid != std::process::id()
                && paths.iter().any(|path| {
                    path.rsplit('\\')
                        .next()
                        .is_some_and(|exe| exe.eq_ignore_ascii_case(&name))
                })
                && let Ok(path) =
                    process(pid).and_then(|process| process_path(&process, &mut buffer))
                && paths
                    .iter()
                    .any(|wanted| wanted.eq_ignore_ascii_case(&path))
            {
                found.push((entry.th32ParentProcessID, Application { name, path, pid }));
            }
            next = Process32NextW(handle, &mut entry);
        }
    }
    // Browser audio often lives in children with the same executable name.
    let children: Vec<u32> = found
        .iter()
        .filter_map(|(parent, child)| {
            found
                .iter()
                .any(|(_, app)| app.pid == *parent && app.path.eq_ignore_ascii_case(&child.path))
                .then_some(child.pid)
        })
        .collect();
    found
        .into_iter()
        .map(|(_, app)| app)
        .filter(|app| !children.contains(&app.pid))
        .collect()
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Completion {
    completed: Arc<AtomicBool>,
    _params: Box<AUDIOCLIENT_ACTIVATION_PARAMS>,
    _variant: Box<ManuallyDrop<PROPVARIANT>>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        self.completed.store(true, Ordering::Release);
        Ok(())
    }
}

pub struct ProcessAudio {
    process: OwnedHandle,
    activation: Option<IActivateAudioInterfaceAsyncOperation>,
    completed: Arc<AtomicBool>,
    deadline: Instant,
}

impl ProcessAudio {
    pub fn start(app: &Application) -> Result<Self> {
        let process = process(app.pid)?;
        let mut buffer = vec![0; 32_768];
        anyhow::ensure!(
            process_path(&process, &mut buffer)?.eq_ignore_ascii_case(&app.path),
            "Application process changed"
        );
        let mut params = Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: app.pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        });
        // The blob borrows the boxed parameters. PropVariantClear would free them a second time.
        let variant = Box::new(ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: (&raw mut *params).cast(),
                        },
                    },
                    ..Default::default()
                }),
            },
        }));
        let activation = std::ptr::from_ref(&**variant);
        let completed = Arc::new(AtomicBool::new(false));
        // The callback owns the activation payload for the whole asynchronous request.
        let handler: IActivateAudioInterfaceCompletionHandler = Completion {
            completed: completed.clone(),
            _params: params,
            _variant: variant,
        }
        .into();
        let operation = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(activation),
                &handler,
            )?
        };
        Ok(Self {
            process,
            activation: Some(operation),
            completed,
            deadline: Instant::now() + Duration::from_secs(5),
        })
    }

    pub fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) == WAIT_TIMEOUT }
    }

    pub fn client(&mut self) -> Result<Option<IAudioClient>> {
        if !self.completed.load(Ordering::Acquire) {
            anyhow::ensure!(
                Instant::now() < self.deadline,
                "WASAPI activation timed out"
            );
            return Ok(None);
        }
        let operation = self
            .activation
            .take()
            .context("WASAPI activation already collected")?;
        let mut status = HRESULT(0);
        let mut interface = None;
        unsafe { operation.GetActivateResult(&mut status, &mut interface)? };
        status.ok()?;
        Ok(Some(
            interface
                .context("WASAPI activation returned no interface")?
                .cast()?,
        ))
    }
}

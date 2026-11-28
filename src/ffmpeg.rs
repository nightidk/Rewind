use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;

use crate::recorder::{Process, command};

const URLS: [&str; 2] = [
    "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.1-essentials_build.7z",
    "https://github.com/GyanD/codexffmpeg/releases/download/9.1/ffmpeg-9.1-essentials_build.7z",
];
const SHA256: &str = "835b9a733445ba7a56a37a26cf7bde84e2f3ce7da2a8ec984276bb2abb1b922c";
const ARCHIVE_SIZE: u64 = 36_671_021;
const ENTRY: &str = "ffmpeg-9.1-essentials_build/bin/ffmpeg.exe";

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Progress {
    Connecting {
        mirror: bool,
    },
    Downloading {
        bytes: u64,
        total: u64,
        remaining: Option<u64>,
    },
    Checking,
    Extracting,
}

pub fn find(preferred: &Path, data: &Path) -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("ffmpeg.exe")));
    let path = std::env::var_os("PATH").unwrap_or_default();
    [Some(preferred.to_owned()), beside]
        .into_iter()
        .flatten()
        .chain(
            std::env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join("ffmpeg.exe")),
        )
        .chain([data.join("tools").join("ffmpeg.exe")])
        .find(|path| path.is_file())
}

pub(crate) fn system_directory() -> Result<PathBuf> {
    let mut buffer = [0; 1024];
    let len = unsafe { GetSystemDirectoryW(Some(&mut buffer)) as usize };
    ensure!(
        len > 0 && len < buffer.len(),
        "Не удалось найти системную папку Windows."
    );
    Ok(OsString::from_wide(&buffer[..len]).into())
}

fn run(command: &mut Command, stop: &AtomicBool, mut poll: impl FnMut()) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "Установка FFmpeg отменена.");
    let mut process = Process::start(command)?;
    while !stop.load(Ordering::Relaxed) {
        poll();
        if let Some(status) = process.child.try_wait()? {
            ensure!(status.success(), "Код завершения {status}.");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("Установка FFmpeg отменена.")
}

fn download(
    curl: &Path,
    archive: &Path,
    log: &Path,
    stop: &AtomicBool,
    report: &mut impl FnMut(Progress),
) -> Result<()> {
    for (attempt, url) in URLS.into_iter().enumerate() {
        ensure!(!stop.load(Ordering::Relaxed), "Установка FFmpeg отменена.");
        File::create(archive)?;
        let started = Instant::now();
        let mut last_advance = started;
        let mut next_report = started;
        let mut bytes = 0;
        let mut previous = Progress::Connecting {
            mirror: attempt != 0,
        };
        report(previous);
        let result = run(
            command(curl)
                .args([
                    "--disable",
                    "--silent",
                    "--show-error",
                    "--fail",
                    "--location",
                    "--proto",
                    "=https",
                    "--proto-redir",
                    "=https",
                    "--connect-timeout",
                    "15",
                    "--max-time",
                    "300",
                    "--speed-limit",
                    "1024",
                    "--speed-time",
                    "25",
                    "--max-filesize",
                    "50000000",
                    "--output",
                ])
                .arg(archive)
                .arg(url)
                .stdout(Stdio::null())
                .stderr(File::create(log)?)
                .stdin(Stdio::null()),
            stop,
            || {
                let now = Instant::now();
                if now < next_report {
                    return;
                }
                next_report = now + Duration::from_millis(350);
                let received = File::open(archive)
                    .and_then(|file| file.metadata())
                    .map_or(bytes, |metadata| metadata.len().min(ARCHIVE_SIZE));
                if received > bytes {
                    bytes = received;
                    last_advance = now;
                }
                if bytes == 0 {
                    return;
                }
                let remaining =
                    (now.duration_since(last_advance) < Duration::from_secs(5)).then(|| {
                        ((ARCHIVE_SIZE - bytes) as f64 * now.duration_since(started).as_secs_f64()
                            / bytes as f64)
                            .ceil() as u64
                    });
                let progress = Progress::Downloading {
                    bytes,
                    total: ARCHIVE_SIZE,
                    remaining,
                };
                if progress != previous {
                    previous = progress;
                    report(progress);
                }
            },
        );
        if result.is_ok() {
            return Ok(());
        }
    }
    anyhow::bail!(
        "Не удалось скачать FFmpeg. Проверьте соединение и попробуйте снова. {}",
        fs::read_to_string(log).unwrap_or_default().trim()
    )
}

pub fn install(
    data: &Path,
    stop: &AtomicBool,
    mut report: impl FnMut(Progress),
) -> Result<PathBuf> {
    let system = system_directory()?;
    let tools = data.join("tools");
    fs::create_dir_all(&tools).context("Не удалось создать папку для FFmpeg")?;
    let work = tempfile::Builder::new()
        .prefix("ffmpeg-")
        .tempdir_in(&tools)?;
    let archive = work.path().join("ffmpeg.7z");
    download(
        &system.join("curl.exe"),
        &archive,
        &work.path().join("download.log"),
        stop,
        &mut report,
    )?;
    report(Progress::Checking);
    let mut hash = Sha256::new();
    io::copy(&mut BufReader::new(File::open(&archive)?), &mut hash)?;
    ensure!(
        format!("{:x}", hash.finalize()) == SHA256,
        "Контрольная сумма архива FFmpeg не совпала. Загрузка отклонена."
    );
    report(Progress::Extracting);

    let executable = tempfile::Builder::new()
        .suffix(".exe")
        .tempfile_in(&tools)?;
    run(
        command(&system.join("tar.exe"))
            .args(["-xOf"])
            .arg(&archive)
            .arg(ENTRY)
            .stdout(executable.reopen()?)
            .stderr(Stdio::null())
            .stdin(Stdio::null()),
        stop,
        || {},
    )
    .context("Не удалось извлечь ffmpeg.exe из архива")?;
    ensure!(
        executable.as_file().metadata()?.len() > 0,
        "Распакованный ffmpeg.exe пуст."
    );
    executable.as_file().sync_all()?;
    ensure!(!stop.load(Ordering::Relaxed), "Установка FFmpeg отменена.");
    let destination = tools.join("ffmpeg.exe");
    executable.persist_noclobber(&destination)?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "downloads the pinned archive over HTTPS"]
    fn installs_pinned_build_in_app_directory() -> Result<()> {
        let temp = tempfile::tempdir_in(std::env::current_dir()?.join("target"))?;
        let mut progress = Vec::new();
        let path = install(
            &temp.path().canonicalize()?,
            &AtomicBool::new(false),
            |state| progress.push(state),
        )?;
        assert!(
            progress
                .iter()
                .any(|state| matches!(state, Progress::Downloading { bytes, .. } if *bytes > 0))
        );
        assert!(progress.ends_with(&[Progress::Checking, Progress::Extracting]));
        let output = command(&path).args(["-hide_banner", "-filters"]).output()?;
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("ddagrab"));
        assert_eq!(
            path,
            temp.path().canonicalize()?.join("tools").join("ffmpeg.exe")
        );
        Ok(())
    }
}

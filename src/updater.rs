use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::download_progress::{ProgressMeter, Transfer};
use crate::ffmpeg::system_directory;
use crate::recorder::{Process, command};

const RELEASES: &str = "https://github.com/nightidk/Rewind/releases";
const UPDATE_MANIFEST: &str = "manifest.json";
const MAX_MANIFEST: usize = 4096;
const MAX_EXE: u64 = 128 * 1024 * 1024;

#[derive(Deserialize)]
struct Manifest {
    size: u64,
    sha256: String,
}

#[derive(Clone, Serialize)]
pub struct Update {
    pub version: String,
    #[serde(skip)]
    digest: String,
    #[serde(skip)]
    size: u64,
}

pub struct Download {
    folder: TempDir,
    digest: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Progress {
    Connecting,
    Downloading {
        #[serde(flatten)]
        transfer: Transfer,
    },
    Checking,
}

fn curl(url: &str, timeout: &str) -> Result<Command> {
    let mut cmd = command(&system_directory()?.join("curl.exe"));
    cmd.args([
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
        "10",
        "--max-time",
        timeout,
        "--max-redirs",
        "5",
        "--user-agent",
        "Rewind",
    ])
    .arg(url);
    Ok(cmd)
}

pub fn check() -> Result<Option<Update>> {
    let output = curl(&format!("{RELEASES}/latest"), "30")?
        .args([
            "--head",
            "--output",
            "NUL",
            "--write-out",
            "%{url_effective}",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "GitHub Releases: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let Some(version) = newer_release(
        std::str::from_utf8(&output.stdout)?,
        &Version::parse(env!("CARGO_PKG_VERSION"))?,
    )?
    else {
        return Ok(None);
    };
    let output = curl(&asset_url(&version, UPDATE_MANIFEST), "30")?
        .arg("--max-filesize")
        .arg(MAX_MANIFEST.to_string())
        .output()?;
    ensure!(
        output.status.success(),
        "Could not download {UPDATE_MANIFEST}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    parse_manifest(version, &output.stdout).map(Some)
}

fn newer_release(url: &str, current: &Version) -> Result<Option<Version>> {
    let version = url
        .trim()
        .strip_prefix(RELEASES)
        .and_then(|path| path.strip_prefix("/tag/v"))
        .context("Unexpected GitHub release URL")?;
    let version = Version::parse(version).context("Invalid release tag")?;
    if !version.pre.is_empty() || !version.build.is_empty() || &version <= current {
        return Ok(None);
    }
    Ok(Some(version))
}

fn asset_url(version: impl std::fmt::Display, asset: &str) -> String {
    format!("{RELEASES}/download/v{version}/{asset}")
}

fn parse_manifest(version: Version, bytes: &[u8]) -> Result<Update> {
    ensure!(bytes.len() <= MAX_MANIFEST, "Update manifest is too large");
    let manifest: Manifest = serde_json::from_slice(bytes).context("Invalid update manifest")?;
    ensure!(
        manifest.sha256.len() == 64 && manifest.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid Rewind.exe SHA-256"
    );
    ensure!(
        (1..=MAX_EXE).contains(&manifest.size),
        "Invalid Rewind.exe size"
    );
    Ok(Update {
        version: version.to_string(),
        digest: manifest.sha256.to_ascii_lowercase(),
        size: manifest.size,
    })
}

pub fn download(update: &Update, mut report: impl FnMut(Progress)) -> Result<Download> {
    let exe = std::env::current_exe()?;
    let folder = tempfile::Builder::new()
        .prefix(".rewind-update-")
        .tempdir_in(exe.parent().context("Missing application directory")?)?;
    let path = folder.path().join("Rewind.exe");
    let url = asset_url(&update.version, "Rewind.exe");
    let log = folder.path().join("download.log");
    let mut transfer = ProgressMeter::new(update.size);
    let mut process = Process::start(
        curl(&url, "300")?
            .args([
                "--speed-limit",
                "1024",
                "--speed-time",
                "25",
                "--max-filesize",
            ])
            .arg(update.size.to_string())
            .arg("--output")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&log)?),
    )?;
    let status = loop {
        if let Some(transfer) = transfer.sample(&path) {
            report(Progress::Downloading { transfer });
        }
        if let Some(status) = process.child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    ensure!(
        status.success(),
        "{}",
        fs::read_to_string(&log).unwrap_or_default().trim()
    );
    fs::remove_file(log)?;
    report(Progress::Checking);
    verify(&path, update.size, &update.digest)?;
    Ok(Download {
        folder,
        digest: update.digest.clone(),
    })
}

fn verify(path: &Path, size: u64, digest: &str) -> Result<()> {
    ensure!(
        fs::metadata(path)?.len() == size,
        "Rewind.exe size does not match GitHub Releases"
    );
    let mut hash = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hash)?;
    ensure!(
        format!("{:x}", hash.finalize()) == digest,
        "Rewind.exe SHA-256 does not match GitHub Releases"
    );
    Ok(())
}

impl Download {
    pub fn install(self) -> Result<()> {
        let exe = std::env::current_exe()?;
        let output = command(&system_directory()?.join("WindowsPowerShell/v1.0/powershell.exe"))
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                INSTALL,
            ])
            .env("REWIND_UPDATE_EXE", exe)
            .env("REWIND_UPDATE_DIRECTORY", self.folder.path())
            .env("REWIND_UPDATE_DIGEST", self.digest)
            .env("REWIND_UPDATE_PID", std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        output.context("Could not launch the Rewind update installer")?;
        let _ = self.folder.into_path();
        Ok(())
    }
}

const INSTALL: &str = r#"
$ErrorActionPreference = 'Stop'
$exe = $env:REWIND_UPDATE_EXE
$folder = $env:REWIND_UPDATE_DIRECTORY
$staged = [IO.Path]::Combine($folder, 'Rewind.exe')
$backup = [IO.Path]::Combine($folder, 'previous.exe')
try {
    $running = Get-Process -Id ([int]$env:REWIND_UPDATE_PID) -ErrorAction SilentlyContinue
    if ($running -and -not $running.WaitForExit(30000)) { throw 'Rewind is still running.' }
    $stream = [IO.File]::OpenRead($staged)
    try {
        $sha256 = [Security.Cryptography.SHA256]::Create()
        try {
            $digest = [BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-', '')
        } finally {
            $sha256.Dispose()
        }
    } finally {
        $stream.Dispose()
    }
    if ($digest -ne $env:REWIND_UPDATE_DIGEST) {
        throw 'Update checksum mismatch.'
    }
    [IO.File]::Replace($staged, $exe, $backup)
    try {
        $process = [Diagnostics.ProcessStartInfo]::new($exe)
        $process.WorkingDirectory = [IO.Path]::GetDirectoryName($exe)
        $process.UseShellExecute = $false
        [void][Diagnostics.Process]::Start($process)
    } catch {
        [IO.File]::Replace($backup, $exe, $staged)
        throw
    }
    [IO.File]::Delete($backup)
} catch {
    Add-Type -AssemblyName System.Windows.Forms
    [void][Windows.Forms.MessageBox]::Show($_.Exception.Message, 'Rewind update')
} finally {
    if (-not [IO.File]::Exists($backup)) {
        [IO.File]::Delete($staged)
        [IO.Directory]::Delete($folder)
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_release_requires_a_newer_stable_tag_from_the_rewind_repo() -> Result<()> {
        let current = Version::new(0, 9, 0);
        for tag in ["v0.8.0", "v0.9.0", "v1.0.0-beta.1", "v1.0.0+build"] {
            assert!(newer_release(&format!("{RELEASES}/tag/{tag}"), &current)?.is_none());
        }
        assert_eq!(
            newer_release(&format!("{RELEASES}/tag/v0.10.0"), &current)?,
            Some(Version::new(0, 10, 0))
        );
        for url in [
            format!("{RELEASES}/latest"),
            format!("{RELEASES}/tag/v0.10.0/extra"),
            "https://github.com/another/Rewind/releases/tag/v0.10.0".into(),
        ] {
            assert!(newer_release(&url, &current).is_err());
        }
        Ok(())
    }

    #[test]
    fn manifest_requires_a_valid_size_and_sha256() -> Result<()> {
        let version = Version::new(0, 10, 0);
        let digest = "AB".repeat(32);
        let json = |size, digest: &str| serde_json::json!({ "size": size, "sha256": digest });
        let update = parse_manifest(version.clone(), &serde_json::to_vec(&json(42, &digest))?)?;
        assert_eq!(update.size, 42);
        assert_eq!(update.digest, digest.to_ascii_lowercase());
        assert_eq!(
            asset_url(&update.version, UPDATE_MANIFEST),
            format!("{RELEASES}/download/v0.10.0/manifest.json")
        );
        for invalid in [
            json(0, &digest),
            json(MAX_EXE + 1, &digest),
            json(42, &"g".repeat(64)),
        ] {
            assert!(parse_manifest(version.clone(), &serde_json::to_vec(&invalid)?).is_err());
        }
        assert!(parse_manifest(version, &vec![b' '; MAX_MANIFEST + 1]).is_err());
        Ok(())
    }
}

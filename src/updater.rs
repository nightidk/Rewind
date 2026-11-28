use std::fs::{self, File};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::ffmpeg::system_directory;
use crate::recorder::command;

const REPOSITORY: &str = "nightidk/Rewind";
const MAX_RELEASE_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_EXE: u64 = 128 * 1024 * 1024;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
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

fn curl(url: &str) -> Result<Command> {
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
        "120",
        "--max-redirs",
        "5",
        "--user-agent",
        "Rewind",
    ])
    .arg(url);
    Ok(cmd)
}

pub fn check() -> Result<Option<Update>> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let output = curl(&url)?
        .arg("--max-filesize")
        .arg(MAX_RELEASE_METADATA_BYTES.to_string())
        .output()?;
    ensure!(
        output.status.success(),
        "GitHub Releases: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let release: Release = serde_json::from_slice(&output.stdout)?;
    prepare(release, &Version::parse(env!("CARGO_PKG_VERSION"))?)
}

fn prepare(release: Release, current: &Version) -> Result<Option<Update>> {
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .context("Invalid release tag")?,
    )?;
    if !version.pre.is_empty() || !version.build.is_empty() || &version <= current {
        return Ok(None);
    }
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == "Rewind.exe")
        .context("Rewind.exe is missing from the release")?;
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .context("GitHub did not provide a SHA-256 digest")?;
    ensure!(
        (1..=MAX_EXE).contains(&asset.size),
        "Invalid Rewind.exe size"
    );
    Ok(Some(Update {
        version: version.to_string(),
        digest: digest.to_ascii_lowercase(),
        size: asset.size,
    }))
}

pub fn download(update: &Update) -> Result<Download> {
    let exe = std::env::current_exe()?;
    let folder = tempfile::Builder::new()
        .prefix(".rewind-update-")
        .tempdir_in(exe.parent().context("Missing application directory")?)?;
    let path = folder.path().join("Rewind.exe");
    let url = format!(
        "https://github.com/{REPOSITORY}/releases/download/v{}/Rewind.exe",
        update.version
    );
    let output = curl(&url)?
        .arg("--max-filesize")
        .arg(update.size.to_string())
        .arg("--output")
        .arg(&path)
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
    if ((Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash -ne $env:REWIND_UPDATE_DIGEST) {
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
    fn update_requires_a_newer_stable_version_and_a_digest() -> Result<()> {
        let current = Version::new(0, 9, 0);
        let release = |tag: &str, digest: Option<String>| Release {
            tag_name: tag.into(),
            assets: vec![Asset {
                name: "Rewind.exe".into(),
                size: 42,
                digest,
            }],
        };
        let digest = format!("sha256:{}", "a".repeat(64));
        assert!(prepare(release("v0.8.0", None), &current)?.is_none());
        assert!(prepare(release("v1.0.0-beta.1", None), &current)?.is_none());
        assert!(prepare(release("v0.10.0", None), &current).is_err());
        assert_eq!(
            prepare(release("v0.10.0", Some(digest)), &current)?
                .unwrap()
                .version,
            "0.10.0"
        );
        Ok(())
    }
}

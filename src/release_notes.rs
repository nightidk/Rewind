use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::Result;
use semver::Version;
use serde::Serialize;

const MARKER: &str = "release-notes-version";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize)]
pub struct Release {
    pub version: &'static str,
    changes: Vec<&'static str>,
}

pub fn history() -> Vec<Release> {
    include_str!("../CHANGELOG.md")
        .split("\n## ")
        .skip(1)
        .filter_map(|section| {
            let mut lines = section.lines();
            let version = lines.next()?.trim();
            Version::parse(version).ok()?;
            let changes = lines.filter_map(|line| line.strip_prefix("- ")).collect();
            Some(Release { version, changes })
        })
        .collect()
}

pub fn unread(directory: &Path) -> Result<bool> {
    let current = Version::parse(CURRENT)?;
    match fs::read_to_string(directory.join(MARKER)) {
        Ok(saved) => {
            if let Ok(saved) = Version::parse(saved.trim()) {
                return Ok(saved < current);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // An earlier Rewind may have created its WebView profile before saving any settings.
    if directory.join("webview").is_dir() {
        return Ok(true);
    }
    mark_read(directory)?;
    Ok(false)
}

pub fn mark_read(directory: &Path) -> Result<()> {
    let mut marker = tempfile::NamedTempFile::new_in(directory)?;
    marker.write_all(CURRENT.as_bytes())?;
    marker.as_file().sync_all()?;
    marker.persist(directory.join(MARKER))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_profiles_skip_the_prompt_and_updates_wait_for_dismissal() -> Result<()> {
        let fresh = tempfile::tempdir()?;
        assert!(!unread(fresh.path())?);
        assert_eq!(fs::read_to_string(fresh.path().join(MARKER))?, CURRENT);

        let upgrade = tempfile::tempdir()?;
        fs::create_dir(upgrade.path().join("webview"))?;
        assert!(unread(upgrade.path())?);
        assert!(unread(upgrade.path())?);
        mark_read(upgrade.path())?;
        assert!(!unread(upgrade.path())?);

        fs::write(upgrade.path().join(MARKER), "999.0.0")?;
        assert!(!unread(upgrade.path())?);
        Ok(())
    }
}

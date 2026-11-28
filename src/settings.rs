use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::localization::Locale;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloseBehavior {
    #[default]
    Ask,
    Tray,
    Exit,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    #[default]
    System,
    Ru,
    En,
}

impl Language {
    pub fn resolve(self) -> Locale {
        match self {
            Self::Ru => Locale::Ru,
            Self::En => Locale::En,
            Self::System => Locale::system(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub language: Language,
    pub close_behavior: CloseBehavior,
    pub interface_scale: u16,
    pub start_with_windows: bool,
    pub auto_update: bool,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: Language::default(),
            close_behavior: CloseBehavior::default(),
            interface_scale: 100,
            start_with_windows: false,
            auto_update: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoder {
    Cpu,
    Nvidia,
    Amd,
    Intel,
}

impl Encoder {
    pub fn ffmpeg(self) -> &'static str {
        match self {
            Self::Cpu => "libx264",
            Self::Nvidia => "h264_nvenc",
            Self::Amd => "h264_amf",
            Self::Intel => "h264_qsv",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportCodec {
    #[default]
    H264,
    Hevc,
    Av1,
}

impl ExportCodec {
    pub fn label(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "H.265 / HEVC",
            Self::Av1 => "AV1",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    Dxgi,
    Gdi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub name: String,
    pub system: bool,
    pub microphone: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
}

pub const MAX_TRACKS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub preferences: Preferences,
    pub resume_buffer: bool,
    pub seconds: u32,
    pub shortcut: u32,
    pub output: PathBuf,
    pub ffmpeg: PathBuf,
    pub export_codec: ExportCodec,
    pub automatic_capture: bool,
    pub mode: CaptureMode,
    pub display: usize,
    pub native_resolution: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub encoder: Encoder,
    pub cursor: bool,
    pub audio_bitrate: u32,
    pub system_device: String,
    pub microphone_device: String,
    pub system_volume: u32,
    pub microphone_volume: u32,
    pub tracks: Vec<Track>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preferences: Preferences::default(),
            resume_buffer: false,
            seconds: 300,
            shortcut: 10,
            output: dirs::video_dir()
                .or_else(dirs::home_dir)
                .unwrap_or_else(std::env::temp_dir)
                .join("Rewind"),
            ffmpeg: PathBuf::new(),
            export_codec: ExportCodec::default(),
            automatic_capture: true,
            mode: CaptureMode::Dxgi,
            display: 0,
            native_resolution: true,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 16_000,
            encoder: Encoder::Cpu,
            cursor: true,
            audio_bitrate: 192,
            system_device: "default".into(),
            microphone_device: String::new(),
            system_volume: 100,
            microphone_volume: 100,
            tracks: vec![Track {
                name: Locale::system().text("Микс").into_owned(),
                system: true,
                microphone: true,
                application: None,
            }],
        }
    }
}

impl Settings {
    pub fn load(directory: &Path) -> Result<Self> {
        let path = directory.join("settings.json");
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut settings: Self =
                    serde_json::from_slice(&bytes).context("Ошибка чтения настроек")?;
                // Keep the selected app in tracks saved before the sources became exclusive.
                for track in &mut settings.tracks {
                    if track.application.is_some() {
                        track.system = false;
                        track.microphone = false;
                    }
                }
                Ok(settings)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("Ошибка чтения настроек"),
        }
    }

    pub fn write(&self, directory: &Path) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.as_file().sync_all()?;
        file.persist(directory.join("settings.json"))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (10..=1800).contains(&self.seconds),
            "Буфер должен вмещать от 10 до 1800 секунд."
        );
        ensure!(
            (6..=11).contains(&self.shortcut),
            "Выберите клавишу от F6 до F11."
        );
        ensure!(
            self.output.is_absolute(),
            "Укажите полный путь к папке клипов."
        );
        ensure!(
            self.ffmpeg.as_os_str().is_empty() || self.ffmpeg.is_absolute(),
            "Укажите полный путь к ffmpeg.exe."
        );
        ensure!(self.display <= 31, "Номер выхода должен быть от 0 до 31.");
        ensure!(
            self.native_resolution
                || ((160..=7680).contains(&self.width)
                    && (120..=4320).contains(&self.height)
                    && self.width.is_multiple_of(2)
                    && self.height.is_multiple_of(2)),
            "Ширина и высота должны быть чётными и в пределах от 160×120 до 7680×4320."
        );
        ensure!(
            (1..=144).contains(&self.fps),
            "Частота кадров должна быть от 1 до 144."
        );
        ensure!(
            (500..=150_000).contains(&self.bitrate),
            "Битрейт видео должен быть от 500 до 150 000 кбит/с."
        );
        ensure!(
            (64..=320).contains(&self.audio_bitrate),
            "Битрейт аудио должен быть от 64 до 320 кбит/с."
        );
        ensure!(
            self.system_volume <= 200 && self.microphone_volume <= 200,
            "Громкость должна быть от 0 до 200%."
        );
        ensure!(
            self.tracks.len() <= MAX_TRACKS,
            "В одном клипе можно сохранить до десяти дорожек."
        );
        for track in &self.tracks {
            ensure!(
                !track.name.trim().is_empty()
                    && track.name.chars().count() <= 48
                    && !track.name.chars().any(char::is_control),
                "Название дорожки должно содержать от 1 до 48 символов."
            );
            ensure!(
                track.system || track.microphone || track.application.is_some(),
                "Выберите источник для каждой дорожки или удалите пустую строку."
            );
            if let Some(path) = &track.application {
                ensure!(
                    !track.system && !track.microphone,
                    "Для дорожки приложения отключите систему и микрофон."
                );
                ensure!(
                    Path::new(path).is_absolute(),
                    "Укажите полный путь к приложению."
                );
            }
        }
        Ok(())
    }

    pub fn active_tracks(&self) -> impl Iterator<Item = (&Track, bool, bool)> {
        self.tracks.iter().filter_map(|track| {
            let system = track.system && !self.system_device.is_empty();
            let mic = track.microphone && !self.microphone_device.is_empty();
            (system || mic || track.application.is_some()).then_some((track, system, mic))
        })
    }

    pub fn applications(&self) -> Vec<&str> {
        let mut apps: Vec<&str> = Vec::new();
        for app in self
            .tracks
            .iter()
            .filter_map(|track| track.application.as_deref())
        {
            if !apps.iter().any(|path| path.eq_ignore_ascii_case(app)) {
                apps.push(app);
            }
        }
        apps
    }
}

#[cfg(test)]
#[test]
fn legacy_mixed_application_tracks_keep_the_application() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut settings = Settings::default();
    settings.tracks[0].application = Some(
        directory
            .path()
            .join("app.exe")
            .to_string_lossy()
            .into_owned(),
    );
    assert!(settings.validate().is_err());
    settings.write(directory.path())?;
    let loaded = Settings::load(directory.path())?;
    loaded.validate()?;
    assert_eq!(loaded.tracks[0].application, settings.tracks[0].application);
    assert!(!loaded.tracks[0].system && !loaded.tracks[0].microphone);
    Ok(())
}

#[cfg(test)]
#[test]
fn legacy_profile_defaults_to_windows_language_and_the_close_prompt() -> Result<()> {
    let settings: Settings = serde_json::from_str(r#"{"seconds":120,"shortcut":8}"#)?;
    assert_eq!(settings.seconds, 120);
    assert_eq!(settings.shortcut, 8);
    assert_eq!(settings.preferences.language, Language::System);
    assert_eq!(settings.preferences.close_behavior, CloseBehavior::Ask);
    assert!(!settings.preferences.start_with_windows);
    assert!(!settings.resume_buffer);
    Ok(())
}

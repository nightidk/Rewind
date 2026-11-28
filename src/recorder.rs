use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::os::windows::{
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::CommandExt,
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use tempfile::{Builder, TempDir};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::JobObjects::*;
use windows::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW};

use crate::audio::Feed;
use crate::settings::{CaptureMode, Encoder, ExportCodec, Settings};

const SEGMENT: u32 = 2;
const PRUNE_BATCH: u32 = 8;

#[derive(Debug, Clone, Serialize)]
pub struct Display {
    pub label: String,
    pub primary: bool,
    #[serde(skip)]
    pub handle: usize,
    #[serde(skip)]
    pub x: i32,
    #[serde(skip)]
    pub y: i32,
    #[serde(skip)]
    pub width: i32,
    #[serde(skip)]
    pub height: i32,
}

#[derive(Clone)]
enum VideoInput {
    Synthetic,
    Wgc {
        adapter: Option<u32>,
        monitor: usize,
    },
    Dxgi {
        adapter: Option<u32>,
        output: u32,
    },
    Gdi(Display),
}

struct CapturePlan {
    input: VideoInput,
    encoder: Encoder,
}

fn dxgi_output(display: &Display) -> Option<(u32, u32, Option<Encoder>)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
    for index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        for output in 0.. {
            let Ok(screen) = (unsafe { adapter.EnumOutputs(output) }) else {
                break;
            };
            let Ok(desc) = (unsafe { screen.GetDesc() }) else {
                continue;
            };
            if desc.Monitor.0 as usize == display.handle {
                let encoder =
                    unsafe { adapter.GetDesc1().ok() }.and_then(|desc| match desc.VendorId {
                        0x10de => Some(Encoder::Nvidia),
                        0x1002 => Some(Encoder::Amd),
                        0x8086 => Some(Encoder::Intel),
                        _ => None,
                    });
                return Some((index, output, encoder));
            }
        }
    }
    None
}

fn capture_plans(
    settings: &Settings,
    displays: &[Display],
    synthetic: bool,
) -> Result<VecDeque<CapturePlan>> {
    let plan = |input, encoder| CapturePlan { input, encoder };
    if synthetic {
        return Ok([plan(VideoInput::Synthetic, settings.encoder)].into());
    }
    if !settings.automatic_capture && matches!(settings.mode, CaptureMode::Dxgi) {
        return Ok([plan(
            VideoInput::Dxgi {
                adapter: None,
                output: settings.display as u32,
            },
            settings.encoder,
        )]
        .into());
    }
    let display = displays
        .get(settings.display)
        .context("Выбранный монитор отключён")?;
    let output = dxgi_output(display);
    let wgc = VideoInput::Wgc {
        adapter: output.map(|(adapter, _, _)| adapter),
        monitor: display.handle,
    };
    let gdi = VideoInput::Gdi(display.clone());
    if !settings.automatic_capture {
        let input = if matches!(settings.mode, CaptureMode::Wgc) {
            wgc
        } else {
            gdi
        };
        return Ok([plan(input, settings.encoder)].into());
    }
    let mut plans = VecDeque::new();
    if settings.cursor {
        // Windows composites the pointer for WGC, including custom cursors with alpha.
        if let Some((_, _, Some(encoder))) = output {
            plans.push_back(plan(wgc.clone(), encoder));
        }
        plans.push_back(plan(wgc, Encoder::Cpu));
    }
    if let Some((adapter, output, hardware)) = output {
        let dxgi = VideoInput::Dxgi {
            adapter: Some(adapter),
            output,
        };
        if let Some(encoder) = hardware {
            plans.push_back(plan(dxgi.clone(), encoder));
        }
        plans.push_back(plan(dxgi, Encoder::Cpu));
        if let Some(encoder) = hardware {
            plans.push_back(plan(gdi.clone(), encoder));
        }
    }
    plans.push_back(plan(gdi, Encoder::Cpu));
    Ok(plans)
}

pub fn command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.creation_flags(0x0800_0000);
    command
}

pub struct Process {
    pub child: Child,
    _job: OwnedHandle,
}

impl Process {
    pub fn start(command: &mut Command) -> Result<Self> {
        let job = unsafe {
            let handle = CreateJobObjectW(None, None)?;
            let job = OwnedHandle::from_raw_handle(handle.0);
            let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                    ..Default::default()
                },
                ..Default::default()
            };
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of_val(&info) as u32,
            )?;
            job
        };
        let mut child = command.spawn().context("Не удалось запустить процесс")?;
        if let Err(error) = unsafe {
            AssignProcessToJobObject(HANDLE(job.as_raw_handle()), HANDLE(child.as_raw_handle()))
        } {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("Не удалось привязать процесс к Rewind");
        }
        Ok(Self { child, _job: job })
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, PartialEq)]
struct Segment {
    index: u32,
    start: f64,
    end: f64,
}

fn filename(index: u32) -> String {
    format!("chunk-{index:09}.ts")
}

fn parse_segments(csv: &str) -> Vec<Segment> {
    csv.split_inclusive('\n')
        .filter_map(|line| {
            let (name, range) = line.strip_suffix('\n')?.trim_end().split_once(',')?;
            let index = name
                .strip_prefix("chunk-")?
                .strip_suffix(".ts")?
                .parse()
                .ok()?;
            let (start, end) = range.split_once(',')?;
            let (start, end): (f64, f64) = (start.parse().ok()?, end.parse().ok()?);
            (start.is_finite() && end.is_finite() && end > start).then_some(Segment {
                index,
                start,
                end,
            })
        })
        .collect()
}

fn clip_start(segments: &[Segment], seconds: u32) -> usize {
    let cutoff = segments
        .last()
        .map_or(0.0, |last| last.end - f64::from(seconds));
    let first = segments.partition_point(|segment| segment.start < cutoff - 0.04);
    first.min(segments.len().saturating_sub(1))
}

fn log_tail(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map_or(0, |info| info.len());
    let _ = file.seek(SeekFrom::Start(length.saturating_sub(1_600)));
    let mut bytes = Vec::new();
    let _ = file.take(1_600).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).trim().into()
}

struct Session {
    process: Option<Process>,
    audio: Option<Feed>,
    directory: TempDir,
    settings: Settings,
    segments: Vec<Segment>,
    buffer_from: u32,
    pruned_before: u32,
    retired: VecDeque<u32>,
    progress: Arc<CaptureProgress>,
    encoder: Encoder,
    mode: Option<CaptureMode>,
    reset_at: u64,
    level: Option<u16>,
    stop_at: Option<Instant>,
}

#[derive(Default)]
struct CaptureProgress {
    micros: AtomicU64,
    frames: AtomicU64,
    fps: AtomicU16,
}

impl Session {
    fn start(
        root: &Path,
        settings: &Settings,
        plan: CapturePlan,
        microphone_muted: Arc<AtomicBool>,
    ) -> Result<Self> {
        let synthetic = matches!(plan.input, VideoInput::Synthetic);
        let direct = matches!(plan.input, VideoInput::Dxgi { .. } | VideoInput::Wgc { .. })
            && plan.encoder == Encoder::Nvidia
            && settings.native_resolution;
        let directory = Builder::new().prefix("buffer-").tempdir_in(root)?;
        let audio = Feed::spawn(
            settings,
            synthetic,
            &chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
                .to_string(),
            microphone_muted,
        )?;
        let mut cmd = command(&settings.ffmpeg);
        cmd.current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(directory.path().join("capture.log"))?)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostats",
                "-stats_period",
                "0.2",
                "-progress",
                "pipe:1",
                "-filter_complex_threads",
                "2",
                "-filter_threads",
                "2",
            ]);
        let size_filter = if settings.native_resolution {
            "pad=ceil(iw/2)*2:ceil(ih/2)*2".into()
        } else {
            format!(
                "scale={0}:{1}:flags=fast_bilinear:force_original_aspect_ratio=decrease:force_divisible_by=2,pad={0}:{1}:(ow-iw)/2:(oh-ih)/2",
                settings.width, settings.height
            )
        };
        let software_video = format!("{size_filter},setsar=1");
        if let VideoInput::Dxgi {
            adapter: Some(adapter),
            ..
        }
        | VideoInput::Wgc {
            adapter: Some(adapter),
            ..
        } = &plan.input
        {
            cmd.args([
                "-init_hw_device",
                &format!("d3d11va=screen:{adapter}"),
                "-filter_hw_device",
                "screen",
            ]);
        }
        let mut hardware_input = |filter: String, mode| {
            let filter = if direct {
                format!("{filter}[v]")
            } else {
                format!("{filter},hwdownload,format=bgra,{software_video}[v]")
            };
            cmd.args(["-filter_complex", &filter]);
            Some(mode)
        };
        let mode = match plan.input {
            VideoInput::Synthetic => {
                cmd.args([
                    "-re",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!(
                        "color=c=black:s=320x180:r={},select='not(mod(n,3))'",
                        settings.fps
                    ),
                ]);
                None
            }
            VideoInput::Wgc { monitor, .. } => hardware_input(
                format!(
                    "gfxcapture=hmonitor={monitor}:max_framerate={}:capture_cursor={}",
                    settings.fps,
                    u8::from(settings.cursor)
                ),
                CaptureMode::Wgc,
            ),
            VideoInput::Dxgi { output, .. } => hardware_input(
                format!(
                    "ddagrab=output_idx={output}:framerate={}:draw_mouse={}",
                    settings.fps,
                    u8::from(settings.cursor)
                ),
                CaptureMode::Dxgi,
            ),
            VideoInput::Gdi(display) => {
                cmd.args([
                    "-f",
                    "gdigrab",
                    "-framerate",
                    &settings.fps.to_string(),
                    "-draw_mouse",
                    &u8::from(settings.cursor).to_string(),
                    "-offset_x",
                    &display.x.to_string(),
                    "-offset_y",
                    &display.y.to_string(),
                    "-video_size",
                    &format!("{}x{}", display.width, display.height),
                    "-i",
                    "desktop",
                ]);
                Some(CaptureMode::Gdi)
            }
        };
        let filter_input = matches!(mode, Some(CaptureMode::Dxgi | CaptureMode::Wgc));
        if let Some(feed) = &audio {
            let apps = settings.applications();
            let layout = if apps.is_empty() {
                "quad".to_owned()
            } else {
                format!("{}C", feed.channels)
            };
            cmd.args([
                "-f",
                "s16le",
                "-ar",
                "48000",
                "-ac",
                &feed.channels.to_string(),
                "-channel_layout",
                &layout,
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-i",
                &feed.name,
            ]);
            let track_count = settings.active_tracks().count();
            let audio_source = if filter_input { "[0:a:0]" } else { "[1:a:0]" };
            let mut filter = String::new();
            if track_count > 1 {
                let outputs = (0..track_count)
                    .map(|index| format!("[s{index}]"))
                    .collect::<String>();
                filter.push_str(&format!("{audio_source}asplit={track_count}{outputs};"));
            }
            for (index, (track, system, microphone)) in settings.active_tracks().enumerate() {
                let system = f64::from(if system { settings.system_volume } else { 0 }) / 100.0;
                let mic = f64::from(if microphone {
                    settings.microphone_volume
                } else {
                    0
                }) / 100.0;
                if index != 0 {
                    filter.push(';');
                }
                let input = if track_count == 1 {
                    audio_source.into()
                } else {
                    format!("[s{index}]")
                };
                filter.push_str(&input);
                let app = track.application.as_ref().and_then(|path| {
                    apps.iter()
                        .position(|app| app.eq_ignore_ascii_case(path))
                        .map(|index| 4 + index * 2)
                });
                filter.push_str(&audio_route(system, mic, app));
                filter.push_str(&format!("[a{index}]"));
            }
            cmd.args(["-filter_complex", &filter]);
        }
        cmd.args([
            "-map",
            if filter_input { "[v]" } else { "0:v:0" },
            "-fps_mode:v",
            "vfr",
        ]);
        if !direct {
            if !filter_input {
                cmd.args(["-vf", &software_video]);
            }
            cmd.args([
                "-pix_fmt",
                if plan.encoder == Encoder::Intel {
                    "nv12"
                } else {
                    "yuv420p"
                },
            ]);
        }
        for (index, (track, _, _)) in settings.active_tracks().enumerate() {
            cmd.args([
                "-map",
                &format!("[a{index}]"),
                &format!("-metadata:s:a:{index}"),
                &format!("title={}", track.name),
                &format!("-disposition:a:{index}"),
                if index == 0 { "default" } else { "0" },
            ]);
        }
        cmd.args([
            "-c:a",
            "aac",
            "-b:a",
            &format!("{}k", settings.audio_bitrate),
            "-c:v",
            plan.encoder.ffmpeg(),
        ]);
        match plan.encoder {
            Encoder::Cpu => cmd.args([
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-sc_threshold",
                "0",
                "-x264-params",
                // Override the fixed frame duration implied by the zerolatency tune.
                "filler=1:force-cfr=0",
            ]),
            Encoder::Nvidia => {
                cmd.args(["-preset", "p4", "-forced-idr", "1", "-rc", "cbr"]);
                // Older builds enable filler with CBR and do not expose a separate option.
                if command(&settings.ffmpeg)
                    .args(["-hide_banner", "-h", "encoder=h264_nvenc"])
                    .output()
                    .is_ok_and(|help| {
                        String::from_utf8_lossy(&help.stdout).contains("-cbr_padding")
                    })
                {
                    cmd.args(["-cbr_padding", "1"]);
                }
                &mut cmd
            }
            Encoder::Amd => cmd.args([
                "-quality",
                "speed",
                "-rc",
                "cbr",
                "-enforce_hrd",
                "1",
                "-filler_data",
                "1",
            ]),
            Encoder::Intel => cmd.args([
                "-preset",
                "veryfast",
                "-forced_idr",
                "1",
                "-look_ahead",
                "0",
            ]),
        };
        let period = SEGMENT.to_string();
        let bitrate = format!("{}k", settings.bitrate);
        cmd.args([
            "-b:v",
            &bitrate,
            "-minrate",
            &bitrate,
            "-maxrate",
            &bitrate,
            "-bufsize",
            &bitrate,
            "-g",
            &(settings.fps * SEGMENT).to_string(),
            "-bf",
            "0",
            "-force_key_frames",
            &format!("expr:gte(t,n_forced*{SEGMENT})"),
            "-f",
            "segment",
            "-segment_time",
            &period,
            // AAC packets can cross a video keyframe. Keep the same clock in every file.
            "-segment_format_options",
            "mpegts_copyts=1:mpegts_flags=+initial_discontinuity",
            "-segment_time_delta",
            &(0.5 / f64::from(settings.fps)).to_string(),
            "-segment_list",
            "segments.csv",
            "-segment_list_type",
            "csv",
            "-segment_list_size",
            &(settings.seconds.div_ceil(SEGMENT) + 8).to_string(),
            "chunk-%09d.ts",
        ]);
        let mut process = Process::start(&mut cmd)
            .context("Не удалось запустить FFmpeg. Проверьте путь в настройках видео")?;
        let progress = Arc::new(CaptureProgress::default());
        let pipe = process
            .child
            .stdout
            .take()
            .context("FFmpeg не открыл поток состояния")?;
        let progress_reader = Arc::clone(&progress);
        thread::Builder::new()
            .name("rewind-clock".into())
            .spawn(move || {
                let mut frames: u64 = 0;
                let mut previous = (Instant::now(), 0_u64);
                for line in BufReader::new(pipe).lines().map_while(|line| line.ok()) {
                    if let Some(micros) = line
                        .strip_prefix("out_time_us=")
                        .and_then(|value| value.parse::<u64>().ok())
                    {
                        progress_reader.micros.fetch_max(micros, Ordering::Relaxed);
                    } else if let Some(count) = line
                        .strip_prefix("frame=")
                        .and_then(|value| value.parse().ok())
                    {
                        frames = count;
                        progress_reader.frames.store(frames, Ordering::Relaxed);
                    } else if line.starts_with("progress=") {
                        let now = Instant::now();
                        let interval = now.duration_since(previous.0);
                        if interval >= Duration::from_secs(1) {
                            let fps =
                                frames.saturating_sub(previous.1) as f64 / interval.as_secs_f64();
                            progress_reader
                                .fps
                                .store(fps.round() as u16, Ordering::Relaxed);
                            previous = (now, frames);
                        }
                    }
                }
            })?;
        Ok(Self {
            process: Some(process),
            audio,
            directory,
            settings: settings.clone(),
            segments: Vec::new(),
            buffer_from: 0,
            pruned_before: 0,
            retired: VecDeque::new(),
            progress,
            encoder: plan.encoder,
            mode,
            reset_at: 0,
            level: None,
            stop_at: None,
        })
    }

    fn reset_buffer(&mut self) {
        self.reset_at = self.recorded();
        self.level = None;
        if let Some(last) = self.segments.last() {
            self.buffer_from = last.index + 1;
            self.segments.clear();
        }
    }

    fn recorded(&self) -> u64 {
        self.progress.micros.load(Ordering::Relaxed) / 1_000_000
    }

    fn remove_chunk(&self, index: u32) -> Result<()> {
        let path = self.directory.path().join(filename(index));
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("Не удалось очистить старые сегменты буфера"),
        }
    }

    fn release_export(&mut self, pinned: Range<u32>) {
        self.retired
            .extend(pinned.start..pinned.end.min(self.pruned_before));
    }

    fn read_index(&mut self, pinned: Option<&Range<u32>>) -> Result<()> {
        for _ in 0..PRUNE_BATCH {
            let Some(&index) = self.retired.front() else {
                break;
            };
            self.remove_chunk(index)?;
            self.retired.pop_front();
        }
        let path = self.directory.path().join("segments.csv");
        match fs::read_to_string(path) {
            Ok(csv) => {
                let floor = self.pruned_before.max(self.buffer_from);
                let segments: Vec<_> = parse_segments(&csv)
                    .into_iter()
                    .filter(|item| item.index >= floor)
                    .collect();
                if segments.last().map(|item| item.index)
                    >= self.segments.last().map(|item| item.index)
                {
                    self.segments = segments;
                }
                let keep_from = clip_start(&self.segments, self.settings.seconds).saturating_sub(1);
                let prune_through = self
                    .segments
                    .get(keep_from)
                    .map_or(floor, |first| first.index);
                let batch_end = prune_through.min(self.pruned_before.saturating_add(PRUNE_BATCH));
                while self.pruned_before < batch_end {
                    if pinned.is_none_or(|range| !range.contains(&self.pruned_before)) {
                        self.remove_chunk(self.pruned_before)?;
                    }
                    self.pruned_before += 1;
                }
                self.segments.drain(..keep_from);
                Ok(())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error).context("Не удалось прочитать буфер"),
        }
    }

    fn tick(&mut self, pinned: Option<&Range<u32>>) -> Result<()> {
        self.level = self.audio.as_ref().map(Feed::take_level);
        let mut finished = None;
        if let Some(process) = &mut self.process {
            if let Some(status) = process.child.try_wait()? {
                finished = Some(status.success());
            } else if self
                .stop_at
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.process.take();
                finished = Some(true);
            } else if self.stop_at.is_none()
                && let Some(audio) = &mut self.audio
            {
                audio.check()?;
            }
        }
        if let Some(success) = finished {
            self.process.take();
            self.audio.take();
            self.read_index(pinned)?;
            if !success {
                bail!(
                    "Запись прервалась. {}",
                    log_tail(&self.directory.path().join("capture.log"))
                );
            }
        } else if self.process.is_some()
            || self.buffer_from > self.pruned_before
            || !self.retired.is_empty()
        {
            self.read_index(pinned)?;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(process) = &mut self.process
            && self.stop_at.is_none()
        {
            self.stop_at = Some(Instant::now() + Duration::from_secs(3));
            if let Some(mut stdin) = process.child.stdin.take() {
                stdin
                    .write_all(b"q\n")
                    .context("Не удалось остановить FFmpeg")?;
            }
        }
        Ok(())
    }
}

fn audio_route(system: f64, mic: f64, app: Option<usize>) -> String {
    if let Some(app) = app {
        return format!("pan=stereo|c0=c{app}|c1=c{}", app + 1);
    }
    let pan = format!("pan=stereo|c0={system}*c0+{mic}*c2|c1={system}*c1+{mic}*c3");
    if system > 0.0 && mic > 0.0 && system + mic > 1.0 {
        // Float samples retain peaks above full scale until the limiter can attenuate them.
        format!("aformat=sample_fmts=dbl,{pan},alimiter=limit=0.98:level=0:latency=1")
    } else {
        pan
    }
}

struct ExportJob {
    result: Receiver<Result<PathBuf>>,
    pinned: Range<u32>,
}

fn start_export(session: &Session, codec: ExportCodec) -> Result<ExportJob> {
    let segments = &session.segments[clip_start(&session.segments, session.settings.seconds)..];
    let first = segments.first().context("В буфере пока нет записи.")?;
    let last = segments.last().context("В буфере пока нет записи.")?;
    let pinned = first.index..last.index + 1;
    let chunks: Vec<_> = segments.iter().map(|segment| segment.index).collect();
    let working = session.directory.path().to_owned();
    let mut settings = session.settings.clone();
    settings.export_codec = codec;
    let encoder = session.encoder;
    let (result, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("rewind-export".into())
        .spawn(move || {
            let _ = result.send(mux_clip(&working, &settings, encoder, &chunks));
        })?;
    Ok(ExportJob {
        result: receiver,
        pinned,
    })
}

fn export_video_options(cmd: &mut Command, name: &str, bitrate: u32) {
    cmd.creation_flags(CREATE_NO_WINDOW.0 | BELOW_NORMAL_PRIORITY_CLASS.0);
    cmd.args([
        "-c:v",
        name,
        "-pix_fmt",
        if name.ends_with("_qsv") {
            "nv12"
        } else {
            "yuv420p"
        },
    ]);
    cmd.args(match name {
        "hevc_nvenc" | "av1_nvenc" => &["-preset", "p4"][..],
        "hevc_qsv" | "av1_qsv" => &["-preset", "veryfast"],
        "libx265" => &[
            "-preset",
            "ultrafast",
            "-x265-params",
            "pools=2:frame-threads=2:log-level=error",
        ],
        "libsvtav1" => &["-preset", "10", "-svtav1-params", "lp=4"],
        "libaom-av1" => &["-cpu-used", "8", "-threads:v", "4", "-row-mt", "1"],
        _ => &[],
    });
    let bitrate = format!("{bitrate}k");
    cmd.args(["-b:v", &bitrate]);
    if name != "libsvtav1" {
        cmd.args(["-maxrate", &bitrate, "-bufsize", &bitrate]);
    }
}

fn export_encoder(
    settings: &Settings,
    preferred: Encoder,
    log: &Path,
) -> Result<Option<&'static str>> {
    let (hardware, software): (Option<_>, &[_]) = match settings.export_codec {
        ExportCodec::H264 => return Ok(None),
        ExportCodec::Hevc => (
            match preferred {
                Encoder::Nvidia => Some("hevc_nvenc"),
                Encoder::Amd => Some("hevc_amf"),
                Encoder::Intel => Some("hevc_qsv"),
                Encoder::Cpu => None,
            },
            &["libx265"],
        ),
        ExportCodec::Av1 => (
            match preferred {
                Encoder::Nvidia => Some("av1_nvenc"),
                Encoder::Amd => Some("av1_amf"),
                Encoder::Intel => Some("av1_qsv"),
                Encoder::Cpu => None,
            },
            &["libsvtav1", "libaom-av1"],
        ),
    };
    for name in hardware.into_iter().chain(software.iter().copied()) {
        let mut probe = command(&settings.ffmpeg);
        probe
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(log)?)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=s=320x256:r=30",
                "-frames:v",
                "1",
            ]);
        export_video_options(&mut probe, name, settings.bitrate);
        probe.args(["-f", "null", "NUL"]);
        if Process::start(&mut probe)?.child.wait()?.success() {
            return Ok(Some(name));
        }
    }
    bail!(
        "FFmpeg не смог открыть кодировщик {}. {}",
        settings.export_codec.label(),
        log_tail(log)
    );
}

fn mux_clip(
    working: &Path,
    settings: &Settings,
    preferred: Encoder,
    chunks: &[u32],
) -> Result<PathBuf> {
    let mut list = File::create(working.join("clip.list"))?;
    for &index in chunks {
        writeln!(list, "{}", filename(index))?;
    }
    drop(list);
    let output = &settings.output;
    fs::create_dir_all(output)
        .with_context(|| format!("Не удалось создать папку {}", output.display()))?;
    let file = Builder::new()
        .prefix(".rewind-")
        .suffix(".part")
        .tempfile_in(output)?;
    let destination = output.join(format!(
        "Replay-{}.mp4",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S-%3f")
    ));
    let log = working.join("export.log");
    let encoder = export_encoder(settings, preferred, &log)?;
    let mut cmd = command(&settings.ffmpeg);
    cmd.current_dir(working)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(&log)?)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-protocol_whitelist",
            "file,concatf",
            "-i",
            "concatf:clip.list",
            "-map",
            "0:v:0",
            "-map",
            "0:a?",
            "-c:a",
            "copy",
        ]);
    if let Some(encoder) = encoder {
        export_video_options(&mut cmd, encoder, settings.bitrate);
        cmd.args(["-fps_mode:v", "vfr"]);
        if settings.export_codec == ExportCodec::Hevc {
            cmd.args(["-tag:v", "hvc1"]);
        }
    } else {
        cmd.args(["-c:v", "copy"]);
    }
    for (index, (track, _, _)) in settings.active_tracks().enumerate() {
        cmd.arg(format!("-metadata:s:a:{index}"))
            .arg(format!("handler_name={}", track.name))
            .arg(format!("-metadata:s:a:{index}"))
            .arg(format!("title={}", track.name))
            .arg(format!("-disposition:a:{index}"))
            .arg(if index == 0 { "default" } else { "0" });
    }
    cmd.args(["-f", "mp4"]).arg(file.path());
    let mut process = Process::start(&mut cmd)?;
    ensure!(
        process.child.wait()?.success(),
        "Не удалось сохранить клип. {}",
        log_tail(&log)
    );
    file.as_file()
        .sync_all()
        .context("Не удалось сбросить клип на диск")?;
    file.persist_noclobber(&destination)
        .with_context(|| format!("Не удалось записать клип в {}", destination.display()))?;
    Ok(destination)
}

#[derive(Default)]
pub struct Recorder {
    export: Option<ExportJob>,
    session: Option<Session>,
    plans: VecDeque<CapturePlan>,
    saving: Option<(Option<u32>, Instant, ExportCodec)>,
    cycle: u32,
    microphone_muted: Arc<AtomicBool>,
    pub last: Option<PathBuf>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct Status<'a> {
    pub phase: &'static str,
    pub seconds: f64,
    pub recorded: u64,
    pub cycle: u32,
    pub level: Option<u16>,
    pub actual_fps: Option<u16>,
    pub encoder: Option<Encoder>,
    pub capture_mode: Option<CaptureMode>,
    pub limit: Option<u32>,
    pub saving: bool,
    pub microphone_muted: bool,
    pub last: Option<&'a Path>,
    pub error: Option<&'a str>,
}

fn next_session(
    directory: &Path,
    settings: &Settings,
    plans: &mut VecDeque<CapturePlan>,
    microphone_muted: &Arc<AtomicBool>,
) -> Result<Session> {
    let mut error = anyhow::anyhow!("Не найден доступный способ захвата.");
    while let Some(plan) = plans.pop_front() {
        match Session::start(directory, settings, plan, Arc::clone(microphone_muted)) {
            Ok(session) => return Ok(session),
            Err(cause) => error = cause,
        }
    }
    Err(error)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    Failed,
}

pub fn recording_error_text(error: &anyhow::Error) -> String {
    if error
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|error| {
            error.code() == windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED
        })
    {
        "Аудиоустройство недоступно. Проверьте настройки звука.".into()
    } else {
        format!("{error:#}")
    }
}

impl Recorder {
    pub fn start(
        &mut self,
        directory: &Path,
        settings: &Settings,
        displays: &[Display],
        synthetic: bool,
    ) -> Result<()> {
        ensure!(
            self.can_configure(),
            "Сначала дождитесь остановки записи или сохранения клипа."
        );
        settings.validate()?;
        self.error = None;
        self.plans = capture_plans(settings, displays, synthetic)?;
        self.session = Some(next_session(
            directory,
            settings,
            &mut self.plans,
            &self.microphone_muted,
        )?);
        self.cycle = self.cycle.wrapping_add(1);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.plans.clear();
        if let Some(session) = &mut self.session {
            session.stop()?;
        }
        Ok(())
    }

    pub fn toggle_microphone(&self) {
        self.microphone_muted.fetch_xor(true, Ordering::Relaxed);
    }

    pub fn can_configure(&self) -> bool {
        self.saving.is_none()
            && self.export.is_none()
            && self
                .session
                .as_ref()
                .is_none_or(|session| session.process.is_none())
    }

    pub fn save(&mut self, codec: ExportCodec) -> Result<bool> {
        if self.saving.is_some() || self.export.is_some() {
            return Ok(false);
        }
        let session = self.session.as_ref().context("Сначала включите буфер.")?;
        self.error = None;
        if session.process.is_some() {
            self.saving = Some((
                session.segments.last().map(|segment| segment.index),
                Instant::now() + Duration::from_secs(5),
                codec,
            ));
        } else {
            self.export = Some(start_export(session, codec)?);
        }
        Ok(true)
    }

    pub fn tick(&mut self, report_capture_error: impl FnOnce(&str)) -> Option<SaveOutcome> {
        match self.update(report_capture_error) {
            Ok(true) => Some(SaveOutcome::Saved),
            Ok(false) => None,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                Some(SaveOutcome::Failed)
            }
        }
    }

    fn update(&mut self, report_capture_error: impl FnOnce(&str)) -> Result<bool> {
        if let Some(session) = &mut self.session {
            let was_recording = session.process.is_some();
            if let Err(mut error) = session.tick(self.export.as_ref().map(|export| &export.pinned))
            {
                session.process.take();
                session.audio.take();
                let fallback = session.progress.frames.load(Ordering::Relaxed) == 0
                    && session.stop_at.is_none()
                    && !self.plans.is_empty();
                if fallback && let Some(root) = session.directory.path().parent() {
                    match next_session(
                        root,
                        &session.settings,
                        &mut self.plans,
                        &self.microphone_muted,
                    ) {
                        Ok(next) => {
                            self.session = Some(next);
                            self.error = None;
                            return Ok(false);
                        }
                        Err(last_error) => error = last_error,
                    }
                }
                let message = recording_error_text(&error);
                if was_recording {
                    report_capture_error(&message);
                }
                self.error = Some(message);
            }
        }
        if let Some(export) = &self.export {
            let status = export.result.try_recv();
            if !matches!(status, Err(TryRecvError::Empty))
                && let Some(export) = self.export.take()
            {
                if let Some(session) = &mut self.session {
                    session.release_export(export.pinned);
                }
                self.last = Some(status.context("Фоновое сохранение прервалось")??);
                self.cycle = self.cycle.wrapping_add(1);
                if let Some(session) = &mut self.session {
                    session.reset_buffer();
                }
                return Ok(true);
            }
        }
        if let Some(session) = &mut self.session
            && let Some((last, deadline, codec)) = self.saving
            && (session.process.is_none()
                || session.segments.last().map(|segment| segment.index) > last
                || Instant::now() >= deadline)
        {
            self.saving = None;
            self.export = Some(start_export(session, codec)?);
        }
        Ok(false)
    }

    pub fn status(&self) -> Status<'_> {
        let microphone_muted = self.microphone_muted.load(Ordering::Relaxed);
        let Some(session) = &self.session else {
            return Status {
                phase: "idle",
                seconds: 0.0,
                recorded: 0,
                cycle: self.cycle,
                level: None,
                actual_fps: None,
                encoder: None,
                capture_mode: None,
                limit: None,
                saving: false,
                microphone_muted,
                last: self.last.as_deref(),
                error: self.error.as_deref(),
            };
        };
        let recorded = session.recorded();
        let phase = if session.process.is_none() {
            "idle"
        } else if session.stop_at.is_some() {
            "stopping"
        } else if recorded == 0 && session.segments.is_empty() && session.buffer_from == 0 {
            "starting"
        } else {
            "recording"
        };
        let available = session
            .segments
            .last()
            .zip(
                session
                    .segments
                    .get(clip_start(&session.segments, session.settings.seconds)),
            )
            .map_or(0.0, |(last, first)| last.end - first.start);
        let seconds = if phase == "idle" {
            available
        } else {
            recorded.saturating_sub(session.reset_at) as f64
        }
        .min(f64::from(session.settings.seconds));
        Status {
            phase,
            seconds,
            recorded,
            cycle: self.cycle,
            level: session.level,
            actual_fps: (phase == "recording"
                && session.progress.frames.load(Ordering::Relaxed) > 0)
                .then(|| session.progress.fps.load(Ordering::Relaxed)),
            encoder: Some(session.encoder),
            capture_mode: session.mode,
            limit: Some(session.settings.seconds),
            saving: self.export.is_some() || self.saving.is_some(),
            microphone_muted,
            last: self.last.as_deref(),
            error: self.error.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Track;

    #[test]
    fn device_invalidation_has_an_actionable_message() {
        let error = windows::core::Error::from_hresult(
            windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED,
        );
        let error = anyhow::Error::new(error).context("WASAPI");
        assert_eq!(
            recording_error_text(&error),
            "Аудиоустройство недоступно. Проверьте настройки звука."
        );
    }

    #[test]
    fn unfinished_index_line_is_invisible() {
        let input = "chunk-000000001.ts,2.000000,4.000000\r\nchunk-000000002.ts,4.000000,6.000000";
        assert_eq!(
            parse_segments(input),
            [Segment {
                index: 1,
                start: 2.0,
                end: 4.0
            }]
        );
    }

    fn until(
        recorder: &mut Recorder,
        predicate: impl Fn(&Recorder) -> bool,
    ) -> Result<Option<SaveOutcome>> {
        let deadline = Instant::now() + Duration::from_secs(22);
        let mut outcome = None;
        while !predicate(recorder) {
            std::thread::sleep(Duration::from_millis(60));
            outcome = recorder.tick(|_| {}).or(outcome);
            ensure!(
                recorder.error.is_none(),
                "{}",
                recorder.error.as_deref().unwrap_or_default()
            );
            ensure!(
                Instant::now() < deadline,
                "Тайм-аут записи: {} / {}",
                recorder.status().phase,
                recorder.status().seconds
            );
        }
        Ok(outcome)
    }

    #[test]
    #[ignore = "requires FFmpeg"]
    fn mixed_peaks_are_limited_before_pcm_clips() -> Result<()> {
        let ffmpeg = crate::ffmpeg::find(Path::new(""), &std::env::current_dir()?)
            .context("FFmpeg missing")?;
        let tone = "0.8*sin(2*PI*440*t)";
        let source = format!("aevalsrc={tone}|{tone}|{tone}|{tone}:s=48000:d=0.2:c=quad");
        let samples = command(&ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &source,
                "-af",
                &audio_route(1.0, 1.0, None),
                "-f",
                "s16le",
                "pipe:1",
            ])
            .output()?;
        ensure!(
            samples.status.success(),
            "{}",
            String::from_utf8_lossy(&samples.stderr)
        );
        let peak = samples
            .stdout
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
            .max()
            .context("Missing test samples")?;
        assert!((31_000..=32_150).contains(&peak), "mixed peak={peak}");
        Ok(())
    }

    #[test]
    #[ignore = "requires FFmpeg and FFprobe in PATH and runs the capture clock"]
    fn bounded_ring_exports_named_mix_and_separate_tracks() -> Result<()> {
        let temp = tempfile::tempdir_in(std::env::current_dir()?.join("target"))?;
        let mut settings = Settings {
            seconds: 10,
            output: temp.path().join("Клипы с пробелом"),
            width: 640,
            height: 360,
            ffmpeg: crate::ffmpeg::find(&PathBuf::new(), temp.path()).context("FFmpeg missing")?,
            fps: 60,
            bitrate: 16_000,
            export_codec: ExportCodec::Hevc,
            microphone_device: "default".into(),
            tracks: vec![
                Track {
                    name: "Микс".into(),
                    system: true,
                    microphone: true,
                    application: None,
                },
                Track {
                    name: "Система".into(),
                    system: true,
                    microphone: false,
                    application: None,
                },
                Track {
                    name: "Микрофон".into(),
                    system: false,
                    microphone: true,
                    application: None,
                },
            ],
            ..Settings::default()
        };
        settings.tracks.extend((0..7).map(|index| {
            Track {
                name: format!("Приложение {}", index + 1),
                system: false,
                microphone: false,
                application: Some(
                    temp.path()
                        .join(format!("app-{index}.exe"))
                        .to_string_lossy()
                        .into_owned(),
                ),
            }
        }));
        settings.validate()?;
        settings.write(temp.path())?;
        let settings = Settings::load(temp.path())?;
        let mut recorder = Recorder::default();
        recorder.start(temp.path(), &settings, &[], true)?;
        until(&mut recorder, |r| r.status().seconds == 1.0)?;
        assert!(
            recorder
                .session
                .as_ref()
                .is_some_and(|s| s.segments.is_empty())
        );
        assert!(recorder.status().level.is_some_and(|level| level > 1000));
        until(&mut recorder, |r| {
            r.session.as_ref().is_some_and(|s| s.pruned_before >= 1)
        })?;
        assert_eq!(recorder.status().seconds, 10.0);
        let before = recorder.status().recorded;
        let session = recorder
            .session
            .as_ref()
            .context("Missing recording session")?;
        let pinned = session.pruned_before;
        let retained_file = session.directory.path().join(filename(pinned));
        assert!(retained_file.is_file());
        let (pending, receiver) = mpsc::channel();
        recorder.export = Some(ExportJob {
            result: receiver,
            pinned: pinned..pinned + 1,
        });
        until(&mut recorder, |r| {
            r.session.as_ref().is_some_and(|s| s.pruned_before > pinned)
        })?;
        assert!(recorder.status().recorded > before);
        assert!(recorder.status().saving);
        assert_eq!(recorder.status().seconds, 10.0);
        assert!(retained_file.is_file());
        drop(pending);
        assert_eq!(recorder.tick(|_| {}), Some(SaveOutcome::Failed));
        assert_eq!(recorder.tick(|_| {}), None);
        assert!(!retained_file.exists());
        let process_id = recorder
            .session
            .as_ref()
            .and_then(|session| session.process.as_ref())
            .context("Missing capture process")?
            .child
            .id();
        assert!(recorder.save(ExportCodec::H264)?);
        assert!(!recorder.save(ExportCodec::Av1)?);
        assert_eq!(
            until(&mut recorder, |r| r.last.is_some())?,
            Some(SaveOutcome::Saved)
        );
        assert_eq!(recorder.tick(|_| {}), None);
        assert_eq!(recorder.status().phase, "recording");
        assert_eq!(recorder.status().seconds, 0.0);
        assert_eq!(
            recorder
                .session
                .as_ref()
                .and_then(|session| session.process.as_ref())
                .map(|process| process.child.id()),
            Some(process_id)
        );
        let output = command(Path::new("ffprobe.exe"))
            .args([
                "-v",
                "error",
                "-show_streams",
                "-show_format",
                "-show_packets",
                "-show_entries",
                "packet=stream_index,pts:stream:format",
                "-of",
                "json",
            ])
            .arg(recorder.last.as_deref().context("Missing clip")?)
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let probe: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let streams = probe["streams"].as_array().context("Missing streams")?;
        let packets = probe["packets"].as_array().context("Missing packets")?;
        assert_eq!(streams.len(), 11);
        assert_eq!(streams[0]["codec_name"], "h264");
        assert_eq!(streams[0]["width"], 320);
        assert_eq!(streams[0]["height"], 180);
        let video_frames = packets
            .iter()
            .filter(|packet| packet["stream_index"] == 0)
            .count();
        let video_duration: f64 = streams[0]["duration"]
            .as_str()
            .context("Missing video duration")?
            .parse()?;
        let delivered_fps = video_frames as f64 / video_duration;
        assert!(
            (19.0..=21.0).contains(&delivered_fps),
            "delivered FPS={delivered_fps}"
        );
        let bitrate: i64 = streams[0]["bit_rate"]
            .as_str()
            .context("Missing video bitrate")?
            .parse()?;
        eprintln!("Exported video bitrate: {bitrate} bit/s");
        assert!(
            (15_200_000..=16_800_000).contains(&bitrate),
            "video bitrate={bitrate}"
        );
        let mut levels = Vec::new();
        for (index, (stream, (track, _, _))) in streams[1..]
            .iter()
            .zip(settings.active_tracks())
            .enumerate()
        {
            assert_eq!(stream["codec_name"], "aac");
            assert_eq!(stream["tags"]["handler_name"], track.name);
            assert_eq!(stream["disposition"]["default"], u32::from(index == 0));
            let timestamps: Vec<_> = packets
                .iter()
                .filter(|packet| packet["stream_index"] == index + 1)
                .map(|packet| packet["pts"].as_i64().context("Missing audio PTS"))
                .collect::<Result<_>>()?;
            assert!(timestamps.len() > 300);
            assert!(
                timestamps
                    .windows(2)
                    .all(|pair| (pair[1] - pair[0] - 1024).abs() <= 1),
                "Discontinuous AAC timestamps in {}",
                track.name
            );
            let audio = command(&settings.ffmpeg)
                .args(["-v", "error", "-ss", "1", "-i"])
                .arg(recorder.last.as_deref().context("Missing clip")?)
                .args([
                    "-map",
                    &format!("0:a:{index}"),
                    "-t",
                    "0.2",
                    "-ac",
                    "1",
                    "-f",
                    "s16le",
                    "pipe:1",
                ])
                .output()?;
            ensure!(
                audio.status.success(),
                "{}",
                String::from_utf8_lossy(&audio.stderr)
            );
            let level = |frequency: f64| {
                let (real, imaginary) = audio.stdout.chunks_exact(2).enumerate().fold(
                    (0.0, 0.0),
                    |(real, imaginary), (i, b)| {
                        let sample = f64::from(i16::from_le_bytes([b[0], b[1]]));
                        let phase = i as f64 * frequency * std::f64::consts::TAU / 48_000.0;
                        (
                            real + sample * phase.cos(),
                            imaginary + sample * phase.sin(),
                        )
                    },
                );
                real.hypot(imaginary) / (audio.stdout.len() / 2) as f64
            };
            let system_level = level(440.0);
            let mic_level = level(880.0);
            assert_eq!(system_level > 500.0, track.system);
            assert_eq!(mic_level > 500.0, track.microphone);
            if track.application.is_some() {
                assert!(level(index as f64 * 440.0) > 500.0);
            }
            levels.push([system_level, mic_level]);
        }
        for gain in [levels[0][0] / levels[1][0], levels[0][1] / levels[2][1]] {
            assert!((0.95..=1.05).contains(&gain), "mixed-to-solo gain={gain}");
        }
        let duration: f64 = probe["format"]["duration"]
            .as_str()
            .context("Missing duration")?
            .parse()?;
        assert!((8.0..=10.15).contains(&duration), "duration={duration}");
        until(&mut recorder, |r| r.status().seconds > 0.0)?;
        recorder.stop()?;
        until(&mut recorder, Recorder::can_configure)?;
        assert!(recorder.status().seconds > 0.0);
        let seconds = recorder.status().seconds;
        let blocked = temp.path().join("invalid-output");
        fs::write(&blocked, b"")?;
        recorder
            .session
            .as_mut()
            .context("Missing recording session")?
            .settings
            .output = blocked;
        recorder.save(ExportCodec::H264)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut outcome = None;
        while recorder.status().saving && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            outcome = recorder.tick(|_| {}).or(outcome);
        }
        assert_eq!(outcome, Some(SaveOutcome::Failed));
        assert_eq!(recorder.tick(|_| {}), None);
        assert!(!recorder.status().saving);
        assert!(
            recorder
                .error
                .as_deref()
                .is_some_and(|error| error.contains("Не удалось создать папку"))
        );
        assert_eq!(recorder.status().seconds, seconds);
        Ok(())
    }

    #[test]
    #[ignore = "requires FFmpeg with HEVC and AV1 encoders and FFprobe"]
    fn export_selects_mp4_codec_and_preserves_track_names() -> Result<()> {
        let temp = tempfile::tempdir_in(std::env::current_dir()?.join("target"))?;
        let name = " Голос = «чат» ";
        let mut settings = Settings {
            bitrate: 1500,
            ffmpeg: crate::ffmpeg::find(&PathBuf::new(), temp.path()).context("FFmpeg missing")?,
            output: temp.path().join("Клипы"),
            tracks: vec![Track {
                name: name.into(),
                system: true,
                microphone: false,
                application: None,
            }],
            ..Settings::default()
        };
        let source = command(&settings.ffmpeg)
            .current_dir(temp.path())
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x256:rate=30",
                "-f",
                "lavfi",
                "-i",
                "sine=sample_rate=48000",
                "-t",
                "2",
                "-g",
                "30",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-f",
                "segment",
                "-segment_time",
                "1",
                "-segment_format",
                "mpegts",
                "chunk-%09d.ts",
            ])
            .output()?;
        ensure!(
            source.status.success(),
            "{}",
            String::from_utf8_lossy(&source.stderr)
        );
        for preferred in [Encoder::Cpu, Encoder::Nvidia] {
            for (codec, expected) in [
                (ExportCodec::H264, "h264"),
                (ExportCodec::Hevc, "hevc"),
                (ExportCodec::Av1, "av1"),
            ] {
                settings.export_codec = codec;
                let clip = mux_clip(temp.path(), &settings, preferred, &[0, 1])?;
                let probe = command(Path::new("ffprobe.exe"))
                    .args(["-v", "error", "-show_streams", "-of", "json"])
                    .arg(&clip)
                    .output()?;
                ensure!(
                    probe.status.success(),
                    "{}",
                    String::from_utf8_lossy(&probe.stderr)
                );
                let probe: serde_json::Value = serde_json::from_slice(&probe.stdout)?;
                assert_eq!(probe["streams"][0]["codec_name"], expected);
                assert_eq!(probe["streams"][0]["nb_frames"], "60");
                assert_eq!(probe["streams"][1]["codec_name"], "aac");
                assert_eq!(probe["streams"][1]["tags"]["handler_name"], name);
                if codec == ExportCodec::Hevc {
                    assert_eq!(probe["streams"][0]["codec_tag_string"], "hvc1");
                }
                // FFprobe exposes hdlr in stream tags. Check the title stored in the name atom too.
                let mut title = ((8 + name.len()) as u32).to_be_bytes().to_vec();
                title.extend_from_slice(b"name");
                title.extend_from_slice(name.as_bytes());
                assert!(
                    fs::read(clip)?
                        .windows(title.len())
                        .any(|atom| atom == title)
                );
            }
        }
        Ok(())
    }
}

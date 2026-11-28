use std::collections::VecDeque;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU16, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_OUTBOUND, WriteFile};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_NOWAIT, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
};
use windows::core::{GUID, HSTRING, PCWSTR, PWSTR, w};

use crate::applications::{self, ProcessAudio};
use crate::{diagnostics, settings::Settings};

const RATE: i64 = 48_000;
const BLOCK: usize = 480;
const BUFFER_LEAD: i64 = RATE * 120 / 1000;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const PIPE_STALL: Duration = Duration::from_millis(500);
type Stereo = [i16; 2];

struct Com;
impl Com {
    fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}

fn task_string(value: PWSTR) -> Result<String> {
    // Both Core Audio and the property helper allocate these strings with the COM allocator.
    unsafe {
        let result = value.to_string();
        CoTaskMemFree(Some(value.0.cast()));
        Ok(result?)
    }
}

#[derive(Serialize)]
pub struct Device {
    id: String,
    name: String,
}

pub fn devices() -> Result<[Vec<Device>; 2]> {
    let _com = Com::new()?;
    let enumerator = enumerator()?;
    let list = |flow| -> Result<Vec<Device>> {
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
        let count = unsafe { collection.GetCount()? };
        (0..count)
            .map(|index| unsafe {
                let device = collection.Item(index)?;
                let id = task_string(device.GetId()?)?;
                let store = device.OpenPropertyStore(STGM_READ)?;
                let mut value = store.GetValue(&PKEY_Device_FriendlyName)?;
                let name = PropVariantToStringAlloc(&value);
                PropVariantClear(&mut value)?;
                Ok(Device {
                    id,
                    name: task_string(name?)?,
                })
            })
            .collect()
    };
    Ok([list(eRender)?, list(eCapture)?])
}

struct Input {
    capture: IAudioCaptureClient,
    client: IAudioClient,
    queue: PacketQueue,
    _event: Option<OwnedHandle>,
}

struct Source<T> {
    input: Option<T>,
    retry_at: Instant,
    unavailable: bool,
}

impl<T> Source<T> {
    fn new(now: Instant) -> Self {
        Self {
            input: None,
            retry_at: now,
            unavailable: false,
        }
    }

    fn poll(
        &mut self,
        now: Instant,
        open: impl FnOnce() -> Result<T>,
        read: impl FnOnce(&mut T) -> Result<()>,
    ) -> Option<Result<()>> {
        let result = match self.input.as_mut() {
            Some(input) => read(input),
            None if now < self.retry_at => return None,
            None => open().and_then(|mut input| {
                read(&mut input)?;
                self.input = Some(input);
                Ok(())
            }),
        };
        match result {
            Ok(()) => std::mem::take(&mut self.unavailable).then_some(Ok(())),
            Err(error) => {
                self.input = None;
                self.retry_at = now + RECONNECT_INTERVAL;
                (!std::mem::replace(&mut self.unavailable, true)).then_some(Err(error))
            }
        }
    }
}

#[derive(Default)]
struct PacketQueue {
    samples: VecDeque<Stereo>,
    first: Option<i64>,
}

impl PacketQueue {
    fn discard_before(&mut self, position: i64) {
        if let Some(first) = self.first
            && first < position
        {
            let discard = (position - first) as usize;
            if discard >= self.samples.len() {
                self.samples.clear();
                self.first = None;
            } else {
                self.samples.drain(..discard);
                self.first = Some(position);
            }
        }
    }

    fn push(
        &mut self,
        frames: usize,
        sample: impl Fn(usize) -> Stereo,
        position: Option<i64>,
        now: i64,
        cursor: i64,
        discontinuity: bool,
    ) {
        if frames == 0 {
            return;
        }
        let estimated = position.unwrap_or(now - frames as i64);
        if self.first.is_none() && estimated + frames as i64 <= cursor {
            return;
        }
        let first = self.first.get_or_insert(estimated);
        let end = *first + self.samples.len() as i64;
        let delta = estimated - end;
        let interrupted =
            (discontinuity && delta > RATE / 100) || (end <= cursor && delta > RATE / 40);
        let (start, correction) = if interrupted || end < cursor {
            // Loopback may stop sending packets when nothing is playing.
            let gap = (estimated - end.max(cursor)).max(0);
            self.samples
                .resize(self.samples.len() + gap as usize, [0; 2]);
            if end < cursor {
                *first = cursor;
            }
            (end.max(cursor).max(estimated), 0)
        } else {
            // Driver timestamps jitter. Correct persistent drift by at most one sample per packet.
            let correction = if position.is_some() && delta.abs() > RATE * 6 / 1000 {
                delta.signum()
            } else {
                0
            };
            (end, if frames > 1 { correction } else { 0 })
        };
        let length = (frames as i64 + correction) as usize;
        let skipped = (start - estimated).clamp(0, length as i64) as usize;
        for index in 0..length {
            if (interrupted || end < cursor) && index < skipped {
                continue;
            }
            if correction == 0 {
                self.samples.push_back(sample(index));
            } else {
                let denominator = length.saturating_sub(1).max(1);
                let position = index * (frames - 1);
                let base = position / denominator;
                let remainder = position % denominator;
                let a = sample(base);
                let b = sample((base + 1).min(frames - 1));
                self.samples.push_back(std::array::from_fn(|channel| {
                    ((i32::from(a[channel]) * (denominator - remainder) as i32
                        + i32::from(b[channel]) * remainder as i32)
                        / denominator as i32) as i16
                }));
            }
        }
    }

    fn sample(&mut self, position: i64) -> Stereo {
        if self.first != Some(position) {
            return [0; 2];
        }
        match self.samples.pop_front() {
            Some(value) => {
                self.first = Some(position + 1);
                value
            }
            None => [0; 2],
        }
    }
}

impl Input {
    fn start(id: &str, loopback: bool) -> Result<Self> {
        let enumerator = enumerator()?;
        let flow = if loopback { eRender } else { eCapture };
        let device = unsafe {
            if id == "default" {
                enumerator.GetDefaultAudioEndpoint(flow, eConsole)?
            } else {
                let device = enumerator.GetDevice(&HSTRING::from(id))?;
                // A saved device ID must still refer to the selected kind of endpoint.
                let endpoint: IMMEndpoint = windows::core::Interface::cast(&device)?;
                anyhow::ensure!(
                    endpoint.GetDataFlow()? == flow,
                    "Изменился тип аудиоустройства."
                );
                device
            }
        };
        let client = unsafe { device.Activate(CLSCTX_ALL, None)? };
        Self::initialize(client, loopback, false)
    }

    fn initialize(client: IAudioClient, loopback: bool, process: bool) -> Result<Self> {
        let pcm = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: RATE as u32,
            nAvgBytesPerSec: (RATE * 4) as u32,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut flags =
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let float = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xfffe,
                nAvgBytesPerSec: (RATE * 8) as u32,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
                ..pcm
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: 3,
            SubFormat: GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71),
        };
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        unsafe {
            let event = if process {
                // Process loopback can use the engine's native float PCM directly.
                flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
                let handle = CreateEventW(None, false, false, PCWSTR::null())?;
                Some(OwnedHandle::from_raw_handle(handle.0))
            } else {
                None
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                5_000_000,
                0,
                if process { &float.Format } else { &pcm },
                None,
            )?;
            if let Some(event) = &event {
                client.SetEventHandle(HANDLE(event.as_raw_handle()))?;
            }
            let capture = client.GetService()?;
            client.Start()?;
            Ok(Self {
                client,
                capture,
                queue: PacketQueue::default(),
                _event: event,
            })
        }
    }

    fn read(&mut self, origin: i64, now: i64, cursor: i64) -> Result<()> {
        unsafe {
            for _ in 0..64 {
                if self.capture.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags, mut timestamp) = (0, 0, 0);
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    None,
                    Some(&mut timestamp),
                )?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                let position = (timestamp as i64 - origin) * RATE / 10_000_000;
                let reliable = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0
                    && (now - RATE..=now + RATE / 20).contains(&position);
                self.queue.push(
                    frames as usize,
                    |i| {
                        if silent {
                            [0; 2]
                        } else if self._event.is_some() {
                            data.add(i * 8)
                                .cast::<[f32; 2]>()
                                .read_unaligned()
                                .map(|sample| (sample * 32_768.0) as i16)
                        } else {
                            data.add(i * 4).cast::<Stereo>().read_unaligned()
                        }
                    },
                    reliable.then_some(position),
                    now,
                    cursor,
                    flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0,
                );
                self.capture.ReleaseBuffer(frames)?;
                if frames == 0 {
                    break;
                }
            }
        }
        Ok(())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

#[derive(Default)]
struct AppInput {
    input: Option<Input>,
    process: Option<ProcessAudio>,
}

impl AppInput {
    fn read(&mut self, origin: i64, now: i64, cursor: i64) -> Result<()> {
        if let Some(process) = &mut self.process {
            if !process.alive() {
                *self = Self::default();
                return Ok(());
            }
            if self.input.is_none()
                && let Some(client) = process.client()?
            {
                self.input = Some(Input::initialize(client, true, true)?);
            }
        }
        if let Some(input) = &mut self.input {
            input.read(origin, now, cursor)?;
        }
        Ok(())
    }
}

pub struct Feed {
    pub name: String,
    pub channels: usize,
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU16>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Feed {
    pub fn spawn(
        settings: &Settings,
        synthetic: bool,
        tag: &str,
        microphone_muted: Arc<AtomicBool>,
    ) -> Result<Option<Self>> {
        let system = settings.active_tracks().any(|(_, enabled, _)| enabled);
        let mic = settings.active_tracks().any(|(_, _, enabled)| enabled);
        let apps: Vec<String> = settings
            .applications()
            .into_iter()
            .map(str::to_owned)
            .collect();
        if !system && !mic && apps.is_empty() {
            return Ok(None);
        }
        let name = format!(r"\\.\pipe\rewind-{}-{tag}", std::process::id());
        let handle = unsafe {
            let handle = CreateNamedPipeW(
                &HSTRING::from(&name),
                PIPE_ACCESS_OUTBOUND,
                PIPE_TYPE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                524_288,
                0,
                0,
                None,
            );
            if handle.is_invalid() {
                return Err(windows::core::Error::from_win32())
                    .context("Не удалось создать аудиоканал");
            }
            OwnedHandle::from_raw_handle(handle.0)
        };
        let ids = [
            system.then(|| settings.system_device.clone()),
            mic.then(|| settings.microphone_device.clone()),
        ];
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let level = Arc::new(AtomicU16::new(0));
        let level_for_thread = Arc::clone(&level);
        let channels = 4 + apps.len() * 2;
        let thread = thread::spawn(move || {
            run_feed(
                &handle,
                &stop_for_thread,
                &level_for_thread,
                &ids,
                &apps,
                synthetic,
                &microphone_muted,
            )
        });
        Ok(Some(Self {
            name,
            channels,
            stop,
            level,
            thread: Some(thread),
        }))
    }

    pub fn take_level(&self) -> u16 {
        self.level.swap(0, Ordering::Relaxed)
    }

    pub fn check(&mut self) -> Result<()> {
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(thread) = self.thread.take()
        {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("Поток аудио завершился"))??;
            bail!("Поток аудио завершился");
        }
        Ok(())
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_feed(
    handle: &OwnedHandle,
    stop: &AtomicBool,
    level: &AtomicU16,
    ids: &[Option<String>; 2],
    apps: &[String],
    synthetic: bool,
    microphone_muted: &AtomicBool,
) -> Result<()> {
    let _com = Com::new()?;
    struct AudioPriority(HANDLE);
    impl Drop for AudioPriority {
        fn drop(&mut self) {
            let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
        }
    }
    let _priority = unsafe { AvSetMmThreadCharacteristicsW(w!("Audio"), &mut 0) }
        .ok()
        .map(AudioPriority);
    let pipe = HANDLE(handle.as_raw_handle());
    let until = Instant::now() + Duration::from_secs(15);
    while !stop.load(Ordering::Relaxed) {
        match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => break,
            Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => break,
            Err(error)
                if error.code() == ERROR_PIPE_LISTENING.to_hresult() && Instant::now() < until =>
            {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("FFmpeg не подключился к аудиопотоку"),
        }
    }
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut frequency = 0;
    let mut origin = 0;
    unsafe {
        QueryPerformanceFrequency(&mut frequency)?;
        QueryPerformanceCounter(&mut origin)?;
    }
    let clock = |counter: i64| {
        (counter / frequency) * 10_000_000 + (counter % frequency) * 10_000_000 / frequency
    };
    let packet_origin = clock(origin);
    let started_at = Instant::now();
    let mut sources = [Source::new(started_at), Source::new(started_at)];
    let mut app_inputs: Vec<_> = apps.iter().map(|_| AppInput::default()).collect();
    let mut next_scan = started_at;
    let frame_bytes = (2 + apps.len()) * 4;
    let mut data = vec![0_u8; BLOCK * frame_bytes];
    let mut cursor = 0;
    let mut pending = data.len();
    let mut blocked_since: Option<Instant> = None;
    let mut stalled = false;
    let mut silence_until = 0;
    while !stop.load(Ordering::Relaxed) {
        let mut now = 0;
        unsafe { QueryPerformanceCounter(&mut now)? };
        let now = (now - origin) * RATE / frequency;
        if let Some(since) = blocked_since
            && since.elapsed() >= PIPE_STALL
        {
            if !stalled {
                diagnostics::record(
                    "audio",
                    format_args!(
                        "FFmpeg audio pipe is full at {:.2}s; waiting to send silence",
                        cursor as f64 / RATE as f64
                    ),
                );
                stalled = true;
            }
            let due = (now - BUFFER_LEAD) / BLOCK as i64 * BLOCK as i64;
            silence_until = silence_until.max(due).max(cursor + BLOCK as i64);
            // A partially written block must keep its remaining bytes to preserve PCM framing.
            if pending == 0 {
                data.fill(0);
            }
        }
        let source_cursor = cursor.max(silence_until);
        if !synthetic {
            let instant = Instant::now();
            for (index, (source, id)) in sources.iter_mut().zip(ids).enumerate() {
                if let Some(id) = id {
                    // Keep writing PCM for both sources while an endpoint is unavailable.
                    let change = source.poll(
                        instant,
                        || Input::start(id, index == 0),
                        |input| {
                            if source_cursor > cursor {
                                input.queue.discard_before(source_cursor);
                            }
                            input.read(packet_origin, now, source_cursor)
                        },
                    );
                    let label = ["system audio", "microphone"][index];
                    match change {
                        Some(Err(error)) => diagnostics::record(
                            "audio",
                            format_args!("{label} unavailable; recording silence: {error:#}"),
                        ),
                        Some(Ok(())) => {
                            diagnostics::record("audio", format_args!("{label} reconnected"))
                        }
                        None => {}
                    }
                }
            }
            if instant >= next_scan && app_inputs.iter().any(|app| app.process.is_none()) {
                next_scan = instant + RECONNECT_INTERVAL;
                let running = applications::matching(apps);
                for (app, path) in app_inputs.iter_mut().zip(apps) {
                    if app.process.is_none()
                        && let Some(target) = running
                            .iter()
                            .find(|target| path.eq_ignore_ascii_case(&target.path))
                    {
                        app.process = ProcessAudio::start(target).ok();
                    }
                }
            }
            for app in &mut app_inputs {
                if source_cursor > cursor
                    && let Some(input) = &mut app.input
                {
                    input.queue.discard_before(source_cursor);
                }
                if app.read(packet_origin, now, source_cursor).is_err() {
                    *app = AppInput::default();
                }
            }
        }
        if pending < data.len() {
            let mut written = 0;
            unsafe { WriteFile(pipe, Some(&data[pending..]), Some(&mut written), None) }
                .context("FFmpeg отключился от аудиопотока")?;
            pending += written as usize;
            if written == 0 {
                blocked_since.get_or_insert_with(Instant::now);
                thread::sleep(Duration::from_millis(3));
                continue;
            }
            if let Some(since) = blocked_since.take()
                && std::mem::take(&mut stalled)
            {
                diagnostics::record(
                    "audio",
                    format_args!(
                        "FFmpeg is reading audio again after {:.2}s; filling {:.2}s with silence",
                        since.elapsed().as_secs_f64(),
                        (silence_until - cursor) as f64 / RATE as f64
                    ),
                );
            }
            if pending == data.len() {
                cursor += BLOCK as i64;
            }
            continue;
        }
        if now < cursor + BLOCK as i64 + BUFFER_LEAD {
            thread::sleep(Duration::from_millis(3));
            continue;
        }
        if cursor < silence_until {
            data.fill(0);
            pending = 0;
            continue;
        }
        let mut peak = 0;
        let mute_mic = microphone_muted.load(Ordering::Relaxed);
        for (index, row) in data.chunks_exact_mut(frame_bytes).enumerate() {
            let position = cursor + index as i64;
            for (source, bytes) in row.chunks_exact_mut(4).enumerate() {
                let sample = if synthetic {
                    let phase =
                        position as f64 * ((source + 1) as f64 * 440.0) * std::f64::consts::TAU
                            / RATE as f64;
                    [(phase.sin() * 4_000.0) as i16; 2]
                } else {
                    let input = if source < 2 {
                        &mut sources[source].input
                    } else {
                        &mut app_inputs[source - 2].input
                    };
                    input
                        .as_mut()
                        .map_or([0; 2], |input| input.queue.sample(position))
                };
                let sample = if mute_mic && source == 1 {
                    [0; 2]
                } else {
                    sample
                };
                peak = peak
                    .max(sample[0].unsigned_abs())
                    .max(sample[1].unsigned_abs());
                bytes[..2].copy_from_slice(&sample[0].to_le_bytes());
                bytes[2..].copy_from_slice(&sample[1].to_le_bytes());
            }
        }
        level.fetch_max(peak, Ordering::Relaxed);
        pending = 0;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn full_pipe_preserves_the_pcm_timeline_and_resumes_live_audio() -> Result<()> {
        let settings = Settings::default();
        let mut feed = Feed::spawn(&settings, true, "pipe-stall-test", Arc::default())?
            .context("Missing test feed")?;
        let mut input = std::fs::File::open(&feed.name)?;
        let mut block = [0; BLOCK * 8];
        input.read_exact(&mut block)?;
        assert!(block.iter().any(|&byte| byte != 0));

        // Fill the pipe and keep it full beyond the old three-second timeout.
        thread::sleep(Duration::from_millis(5_300));
        feed.check()?;

        let mut silence = 0;
        for _ in 0..700 {
            input.read_exact(&mut block)?;
            if block.iter().all(|&byte| byte == 0) {
                silence += 1;
            } else if silence >= 100 {
                feed.check()?;
                return Ok(());
            }
        }
        bail!(
            "Expected a gap filled with silence followed by live audio, got {silence} silent blocks"
        )
    }

    #[test]
    fn muting_writes_silent_mic_channels_without_interrupting_the_feed() -> Result<()> {
        let settings = Settings {
            microphone_device: "default".into(),
            ..Settings::default()
        };
        let muted = Arc::new(AtomicBool::new(true));
        let feed = Feed::spawn(&settings, true, "microphone-mute-test", Arc::clone(&muted))?
            .context("Missing loopback and microphone feed")?;
        let mut input = std::fs::File::open(&feed.name)?;
        let mut block = [0; BLOCK * 8];
        input.read_exact(&mut block)?;
        assert!(block.chunks_exact(8).all(|frame| frame[4..] == [0; 4]));
        assert!(block.chunks_exact(8).any(|frame| frame[..4] != [0; 4]));

        muted.store(false, Ordering::Relaxed);
        for _ in 0..10 {
            input.read_exact(&mut block)?;
            if block.chunks_exact(8).any(|frame| frame[4..] != [0; 4]) {
                return Ok(());
            }
        }
        bail!("Microphone did not resume on the same audio feed")
    }

    #[test]
    fn failed_endpoint_retries_without_reopening_the_other_source() {
        let now = Instant::now();
        let mut system = Source::new(now);
        let mut mic = Source::new(now);
        system.poll(now, || Ok(1), |_| Ok(()));
        mic.poll(now, || Ok(2), |_| Ok(()));

        let fault = || windows::core::Error::from_hresult(AUDCLNT_E_DEVICE_INVALIDATED).into();
        system.poll(now, || unreachable!(), |_| Err(fault()));
        mic.poll(
            now,
            || unreachable!(),
            |input| {
                *input += 1;
                Ok(())
            },
        );
        assert!(system.input.is_none());
        assert_eq!(mic.input, Some(3));

        system.poll(now, || unreachable!(), |_| unreachable!());
        let retry = now + RECONNECT_INTERVAL;
        system.poll(retry, || Err(fault()), |_| unreachable!());
        assert!(system.input.is_none());
        system.poll(retry + RECONNECT_INTERVAL, || Ok(4), |_| Ok(()));
        assert_eq!(system.input, Some(4));
    }

    #[test]
    fn reopened_endpoint_discards_samples_before_the_output_cursor() {
        let mut queue = PacketQueue::default();
        queue.push(6, |_| [99; 2], Some(9_990), 10_003, 10_000, false);
        queue.push(6, |i| [i as i16 + 1; 2], Some(9_997), 10_003, 10_000, false);
        assert_eq!(queue.sample(10_000), [4; 2]);
        assert_eq!(queue.sample(10_001), [5; 2]);
        assert_eq!(queue.sample(10_002), [6; 2]);
        assert_eq!(queue.sample(10_003), [0; 2]);
    }

    #[test]
    fn packet_timestamp_jitter_does_not_interrupt_pcm() {
        let mut queue = PacketQueue::default();
        for packet in 0..40 {
            let start = packet * BLOCK;
            let jitter = if packet == 0 {
                0
            } else if packet % 2 == 0 {
                144
            } else {
                -144
            };
            queue.push(
                BLOCK,
                |frame| [(start + frame + 1) as i16; 2],
                Some(start as i64 + jitter),
                start as i64 + BLOCK as i64,
                0,
                false,
            );
        }
        for i in 0..(40 * BLOCK) {
            assert_eq!(queue.sample(i as i64), [(i + 1) as i16; 2]);
        }
    }

    #[test]
    fn loopback_can_resume_after_an_interval_without_packets() {
        let mut queue = PacketQueue::default();
        queue.push(BLOCK, |_| [1; 2], Some(0), BLOCK as i64, 0, false);
        for i in 0..BLOCK {
            assert_eq!(queue.sample(i as i64), [1; 2]);
        }
        queue.push(BLOCK, |_| [2; 2], Some(5_000), 5_500, 4_950, true);
        assert_eq!(queue.sample(4_950), [0; 2]);
        for i in 4_951..5_000 {
            assert_eq!(queue.sample(i), [0; 2]);
        }
        assert_eq!(queue.sample(5_000), [2; 2]);
    }
}

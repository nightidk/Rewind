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
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};
use windows::core::{HSTRING, PWSTR, w};

use crate::settings::Settings;

const RATE: i64 = 48_000;
const BLOCK: usize = 480;
const BUFFER_LEAD: i64 = RATE * 120 / 1000;
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
    client: IAudioClient,
    capture: IAudioCaptureClient,
    queue: PacketQueue,
}

#[derive(Default)]
struct PacketQueue {
    samples: VecDeque<Stereo>,
    first: Option<i64>,
}

impl PacketQueue {
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
        let first = self.first.get_or_insert(estimated.max(cursor));
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
        let format = WAVEFORMATEX {
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
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 5_000_000, 0, &format, None)?;
            let capture = client.GetService()?;
            client.Start()?;
            Ok(Self {
                client,
                capture,
                queue: PacketQueue::default(),
            })
        }
    }

    fn read(&mut self, origin: i64, now: i64, cursor: i64) -> Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
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

pub struct Feed {
    pub name: String,
    stop: Arc<AtomicBool>,
    level: Arc<AtomicU16>,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Feed {
    pub fn spawn(settings: &Settings, synthetic: bool, tag: &str) -> Result<Option<Self>> {
        let system = settings.active_tracks().any(|(_, enabled, _)| enabled);
        let mic = settings.active_tracks().any(|(_, _, enabled)| enabled);
        if !system && !mic {
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
        let thread = thread::spawn(move || {
            run_feed(
                &handle,
                &stop_for_thread,
                &level_for_thread,
                &ids,
                synthetic,
            )
        });
        Ok(Some(Self {
            name,
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
    synthetic: bool,
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
    let mut inputs = [None, None];
    if !synthetic {
        for (index, id) in ids.iter().enumerate() {
            if let Some(id) = id {
                inputs[index] = Some(Input::start(id, index == 0).with_context(|| {
                    if index == 0 {
                        "Не удалось захватить системный звук"
                    } else {
                        "Не удалось открыть микрофон"
                    }
                })?);
            }
        }
    }
    let mut data = [0_u8; BLOCK * 8];
    let mut cursor = 0;
    let mut pending = data.len();
    let mut deadline = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut now = 0;
        unsafe { QueryPerformanceCounter(&mut now)? };
        let now = (now - origin) * RATE / frequency;
        for input in inputs.iter_mut().flatten() {
            input.read(clock(origin), now, cursor)?;
        }
        if pending < data.len() {
            let mut written = 0;
            unsafe { WriteFile(pipe, Some(&data[pending..]), Some(&mut written), None) }
                .context("FFmpeg отключился от аудиопотока")?;
            pending += written as usize;
            if pending == data.len() {
                cursor += BLOCK as i64;
            } else if written == 0 {
                if Instant::now() >= deadline {
                    bail!("FFmpeg перестал принимать звук");
                }
                thread::sleep(Duration::from_millis(3));
            }
            continue;
        }
        if now < cursor + BLOCK as i64 + BUFFER_LEAD {
            thread::sleep(Duration::from_millis(3));
            continue;
        }
        let mut peak = 0;
        for (index, row) in data.chunks_exact_mut(8).enumerate() {
            let position = cursor + index as i64;
            for source in 0..2 {
                let sample = if synthetic {
                    let phase = position as f64 * [440.0, 880.0][source] * std::f64::consts::TAU
                        / RATE as f64;
                    [(phase.sin() * 4_000.0) as i16; 2]
                } else {
                    inputs[source]
                        .as_mut()
                        .map_or([0; 2], |input| input.queue.sample(position))
                };
                peak = peak
                    .max(sample[0].unsigned_abs())
                    .max(sample[1].unsigned_abs());
                row[source * 4..source * 4 + 2].copy_from_slice(&sample[0].to_le_bytes());
                row[source * 4 + 2..source * 4 + 4].copy_from_slice(&sample[1].to_le_bytes());
            }
        }
        level.fetch_max(peak, Ordering::Relaxed);
        pending = 0;
        deadline = Instant::now() + Duration::from_secs(3);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Transfer {
    pub bytes: u64,
    pub total: u64,
    pub remaining: Option<u64>,
}

pub struct ProgressMeter {
    started: Instant,
    last_advance: Instant,
    next_sample: Instant,
    progress: Transfer,
}

impl ProgressMeter {
    pub fn new(total: u64) -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last_advance: now,
            next_sample: now,
            progress: Transfer {
                bytes: 0,
                total,
                remaining: None,
            },
        }
    }

    pub fn sample(&mut self, path: &Path) -> Option<Transfer> {
        let now = Instant::now();
        if now < self.next_sample {
            return None;
        }
        self.next_sample = now + Duration::from_millis(350);
        let mut progress = self.progress;
        let bytes = File::open(path)
            .and_then(|file| file.metadata())
            .map_or(progress.bytes, |metadata| {
                metadata.len().min(progress.total)
            });
        if bytes > progress.bytes {
            progress.bytes = bytes;
            self.last_advance = now;
        }
        if progress.bytes == 0 {
            return None;
        }
        progress.remaining =
            (now.duration_since(self.last_advance) < Duration::from_secs(5)).then(|| {
                ((progress.total - progress.bytes) as f64
                    * now.duration_since(self.started).as_secs_f64()
                    / progress.bytes as f64)
                    .ceil() as u64
            });
        if progress == self.progress {
            return None;
        }
        self.progress = progress;
        Some(progress)
    }
}

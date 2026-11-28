use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, mpsc};
use std::thread::{self, JoinHandle};

const MAX_BYTES: u64 = 1024 * 1024;
static EVENTS: Mutex<Option<mpsc::SyncSender<String>>> = Mutex::new(None);

pub struct Journal(Option<JoinHandle<()>>);

impl Journal {
    pub fn start(directory: &Path) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let directory = directory.to_owned();
        let (send, receive) = mpsc::sync_channel::<String>(128);
        let thread = thread::Builder::new()
            .name("rewind-journal".into())
            .spawn(move || {
                for line in receive {
                    if let Err(error) = append(&directory, &line) {
                        eprintln!("Could not write Rewind log: {error}");
                    }
                }
            })?;
        if let Ok(mut events) = EVENTS.lock() {
            *events = Some(send);
        }
        Ok(Self(Some(thread)))
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        if let Ok(mut events) = EVENTS.lock() {
            events.take();
        }
        if let Some(thread) = self.0.take() {
            let _ = thread.join();
        }
    }
}

pub fn record(area: &str, message: fmt::Arguments<'_>) {
    if let Ok(events) = EVENTS.lock()
        && let Some(events) = events.as_ref()
    {
        let message: String = message
            .to_string()
            .chars()
            .take(4_000)
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        let time = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false);
        let _ = events.try_send(format!("{time} [{area}] {message}\n"));
    }
}

fn append(directory: &Path, line: &str) -> io::Result<()> {
    let path = directory.join("rewind.log");
    if fs::metadata(&path).is_ok_and(|file| file.len() + line.len() as u64 > MAX_BYTES) {
        fs::rename(&path, directory.join("rewind.previous.log"))?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

#[test]
fn rotation_replaces_the_previous_log() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let active = directory.path().join("rewind.log");
    let previous = directory.path().join("rewind.previous.log");
    fs::write(&previous, b"old log")?;
    fs::write(&active, vec![b'x'; MAX_BYTES as usize])?;
    append(directory.path(), "new entry\n")?;
    assert_eq!(fs::read_to_string(active)?, "new entry\n");
    assert_eq!(fs::metadata(previous)?.len(), MAX_BYTES);
    Ok(())
}

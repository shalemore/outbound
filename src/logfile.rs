//! The log: one line per message, with the local time. When the file grows too large it becomes
//! `outbound.log.old` and a new one is started, so the last messages are never lost at once.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use log::{Level, LevelFilter, Log, Metadata, Record};
use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_BYTES: u64 = 5 << 20;

struct FileLog(Mutex<Rotating>);

/// A log file that is moved aside, replacing the previous one that was moved aside, once it is
/// larger than `max`.
struct Rotating {
    path: PathBuf,
    file: File,
    written: u64,
    max: u64,
}

impl Rotating {
    fn open(path: &Path, max: u64) -> io::Result<Self> {
        if fs::metadata(path).is_ok_and(|m| m.len() > max) {
            Self::move_aside(path)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
            max,
        })
    }

    fn move_aside(path: &Path) -> io::Result<()> {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        fs::rename(path, old)
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        if self.written > self.max {
            // The old file stays in use if it cannot be moved, for example while someone reads it.
            if Self::move_aside(&self.path).is_ok() {
                self.file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?;
                self.written = 0;
            }
        }
        self.file.write_all(line.as_bytes())?;
        self.written += line.len() as u64;
        Ok(())
    }
}

impl Log for FileLog {
    /// This program's own messages from info up, and only warnings and errors from the libraries.
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Warn
            || (metadata.level() <= Level::Info && metadata.target().starts_with("outbound"))
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // SAFETY: a plain call without arguments.
        let t = unsafe { GetLocalTime() };
        let line = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {:5} {}: {}\n",
            t.wYear,
            t.wMonth,
            t.wDay,
            t.wHour,
            t.wMinute,
            t.wSecond,
            record.level(),
            record.target(),
            record.args()
        );
        if let Ok(mut log) = self.0.lock() {
            let _ = log.write_line(&line);
        }
    }

    fn flush(&self) {}
}

/// Sends the messages of the `log` macros to the file at `path`.
pub fn init(path: &Path) -> io::Result<()> {
    let logger = Box::leak(Box::new(FileLog(Mutex::new(Rotating::open(
        path, MAX_BYTES,
    )?))));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_log_is_moved_aside_and_started_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbound.log");
        let old = dir.path().join("outbound.log.old");
        let mut log = Rotating::open(&path, 20).unwrap();

        log.write_line(
            "first line is long
",
        )
        .unwrap();
        log.write_line(
            "second
",
        )
        .unwrap();
        assert!(!old.exists());
        log.write_line(
            "third
",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&old).unwrap(),
            "first line is long
second
"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "third
"
        );
    }

    #[test]
    fn a_log_that_is_too_large_at_start_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbound.log");
        fs::write(&path, "x".repeat(50)).unwrap();

        let mut log = Rotating::open(&path, 20).unwrap();
        log.write_line(
            "new
",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "new
"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("outbound.log.old"))
                .unwrap()
                .len(),
            50
        );
    }
}

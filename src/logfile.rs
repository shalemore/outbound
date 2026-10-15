//! The log: one line per message, with the local time, in a file that is started over when it
//! grows too large.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::Mutex,
};

use log::{Level, LevelFilter, Log, Metadata, Record};
use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX_BYTES: u64 = 5 << 20;

struct FileLog(Mutex<File>);

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
        if let Ok(mut file) = self.0.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {}
}

/// Sends the messages of the `log` macros to the file at `path`.
pub fn init(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true);
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES) {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    let logger = Box::leak(Box::new(FileLog(Mutex::new(options.open(path)?))));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
    Ok(())
}

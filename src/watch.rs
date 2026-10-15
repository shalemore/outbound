//! Watches the mapped folders, so that a new file is noticed when it appears and not only at the
//! next scan. The scan stays as the safety net: a watcher can miss events, for example on a
//! network drive.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::worker::Command;

pub struct Watch {
    watcher: Option<RecommendedWatcher>,
    folders: HashSet<PathBuf>,
    /// True from the first event until the worker has seen the message about it, so that a flood of
    /// events, such as a download that is being written, sends one message and not thousands.
    announced: Arc<AtomicBool>,
}

impl Watch {
    /// Every change in a watched folder sends `Command::Changed` to `commands`.
    pub fn new(commands: Sender<Command>) -> Self {
        let announced = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&announced);
        let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            // Reading a file changes nothing. An error means that events were lost, which is as
            // good a reason to look again as a change.
            let changed = result
                .map(|event| !matches!(event.kind, EventKind::Access(_) | EventKind::Other))
                .unwrap_or(true);
            if changed && !flag.swap(true, Ordering::AcqRel) {
                let _ = commands.send(Command::Changed);
            }
        });
        let watcher = match watcher {
            Ok(watcher) => Some(watcher),
            Err(e) => {
                log::warn!("cannot watch folders, scanning only: {e}");
                None
            }
        };
        Self {
            watcher,
            folders: HashSet::new(),
            announced,
        }
    }

    /// Called when the worker has received `Command::Changed`, so that the next change is
    /// announced again.
    pub fn acknowledge(&self) {
        self.announced.store(false, Ordering::Release);
    }

    /// Watches exactly these folders, with everything below them. A folder that does not exist yet
    /// is tried again the next time.
    pub fn set_folders(&mut self, wanted: &[PathBuf]) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        let wanted: HashSet<&PathBuf> = wanted.iter().collect();
        self.folders.retain(|folder| {
            let keep = wanted.contains(folder);
            if !keep {
                let _ = watcher.unwatch(folder);
            }
            keep
        });
        for folder in wanted {
            if self.folders.contains(folder) || !folder.is_dir() {
                continue;
            }
            match watcher.watch(folder, RecursiveMode::Recursive) {
                Ok(()) => {
                    self.folders.insert(folder.clone());
                }
                Err(e) => log::warn!("cannot watch {}: {e}", folder.display()),
            }
        }
    }
}

/// Calls `changed` whenever one of `files` is written, created, renamed or deleted, so that an
/// edit by hand takes effect at once. The folders that hold the files are watched, because an
/// editor that saves through a temporary file replaces the file itself. A file in a folder that
/// does not exist is not watched. The watching stops when the returned value is dropped.
pub fn watch_files(
    files: &[PathBuf],
    changed: impl Fn() + Send + 'static,
) -> Option<RecommendedWatcher> {
    let wanted: Vec<PathBuf> = files.to_vec();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        let Ok(event) = result else { return };
        if !matches!(event.kind, EventKind::Access(_) | EventKind::Other)
            && event.paths.iter().any(|path| is_one_of(path, &wanted))
        {
            changed();
        }
    })
    .inspect_err(|e| log::warn!("cannot watch the settings files: {e}"))
    .ok()?;
    let folders: HashSet<&Path> = files.iter().filter_map(|file| file.parent()).collect();
    for folder in folders.into_iter().filter(|folder| folder.is_dir()) {
        if let Err(e) = watcher.watch(folder, RecursiveMode::NonRecursive) {
            log::warn!("cannot watch {}: {e}", folder.display());
        }
    }
    Some(watcher)
}

/// Windows paths compare without regard to case.
fn is_one_of(path: &Path, files: &[PathBuf]) -> bool {
    let path = path.to_string_lossy().to_lowercase();
    files
        .iter()
        .any(|file| file.to_string_lossy().to_lowercase() == path)
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::mpsc, time::Duration};

    use super::*;

    #[test]
    fn a_new_file_is_announced_once() {
        let dir = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut watch = Watch::new(sender);
        watch.set_folders(&[dir.path().to_path_buf()]);

        fs::write(dir.path().join("a.mkv"), "x").unwrap();
        fs::write(dir.path().join("b.mkv"), "y").unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok(Command::Changed)
        ));
        // More changes are not announced again until the worker has seen the first message.
        fs::write(dir.path().join("c.mkv"), "z").unwrap();
        assert!(receiver.recv_timeout(Duration::from_millis(500)).is_err());

        watch.acknowledge();
        fs::write(dir.path().join("d.mkv"), "w").unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok(Command::Changed)
        ));
    }

    #[test]
    fn a_settings_file_is_noticed_and_its_neighbours_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        let (sender, receiver) = mpsc::channel();
        let _watcher = watch_files(std::slice::from_ref(&file), move || {
            let _ = sender.send(());
        })
        .unwrap();

        fs::write(dir.path().join("other.txt"), "x").unwrap();
        assert!(receiver.recv_timeout(Duration::from_millis(500)).is_err());

        // Saved the way many editors do it: written elsewhere, then renamed over the file.
        fs::write(dir.path().join("config.toml.tmp"), "a = 1").unwrap();
        fs::rename(dir.path().join("config.toml.tmp"), &file).unwrap();
        assert!(receiver.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn a_folder_that_is_not_wanted_any_more_is_not_watched() {
        let dir = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut watch = Watch::new(sender);
        watch.set_folders(&[dir.path().to_path_buf()]);
        watch.set_folders(&[]);
        // Windows lets go of a folder a moment after it is no longer watched, and a change in
        // that moment can still be reported.
        std::thread::sleep(Duration::from_millis(500));
        while receiver.try_recv().is_ok() {}
        watch.acknowledge();

        fs::write(dir.path().join("a.mkv"), "x").unwrap();
        assert!(receiver.recv_timeout(Duration::from_millis(500)).is_err());
    }
}

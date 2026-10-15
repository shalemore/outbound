//! Watches the mapped folders, so that a new file is noticed when it appears and not only at the
//! next scan. The scan stays as the safety net: a watcher can miss events, for example on a
//! network drive.

use std::{
    collections::HashSet,
    path::PathBuf,
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
                tracing::warn!("cannot watch folders, scanning only: {e}");
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
                Err(e) => tracing::warn!("cannot watch {}: {e}", folder.display()),
            }
        }
    }
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
    fn a_folder_that_is_not_wanted_any_more_is_not_watched() {
        let dir = tempfile::tempdir().unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut watch = Watch::new(sender);
        watch.set_folders(&[dir.path().to_path_buf()]);
        watch.set_folders(&[]);

        fs::write(dir.path().join("a.mkv"), "x").unwrap();
        assert!(receiver.recv_timeout(Duration::from_millis(500)).is_err());
    }
}

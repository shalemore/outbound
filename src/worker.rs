use std::{
    any::Any,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    config::{self, Config},
    ssh::SshRemote,
    status::Reporter,
    sync::{Engine, Memory, now_secs},
    watch::Watch,
};

const FALLBACK_INTERVAL: Duration = Duration::from_secs(30);
/// How long after the first change a cycle starts, so that the files of a burst are scanned
/// together.
const CHANGE_DELAY: Duration = Duration::from_secs(3);
/// The shortest time between the starts of two cycles that changes cause. A file that is being
/// written changes all the time, and every cycle asks the server about the folders.
const MIN_CYCLE_GAP: Duration = Duration::from_secs(10);

/// Asks the thread that runs the event loop to show a notification: (title, body).
pub type Notifier = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub enum Command {
    /// Start the next cycle now, and compare earlier conflicts again.
    SyncNow,
    /// Start the next cycle now, because the configuration changed. Earlier conflicts are kept.
    Rescan,
    /// Look at the pause switch again.
    Wake,
    /// Something changed in a watched folder.
    Changed,
}

/// The pause switch. The UI thread flips it at once, and an upload that is running notices it
/// within a chunk.
#[derive(Default)]
pub struct Control {
    paused: AtomicBool,
}

impl Control {
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
}

/// Background thread: syncs one cycle when something changed in a folder, when a file that was
/// still being written can be uploaded, and otherwise every interval. "Sync now" starts the next
/// cycle at once and makes earlier conflicts be compared again. While paused it does nothing until
/// it is woken.
///
/// `events` is the sender of `commands`, for the watcher.
pub fn spawn(
    reporter: Reporter,
    commands: Receiver<Command>,
    events: Sender<Command>,
    notify: Notifier,
    control: Arc<Control>,
) {
    thread::spawn(move || {
        let mut memory = Memory::default();
        let mut watch = Watch::new(events);
        loop {
            if control.paused() {
                reporter.update(|status| {
                    status.paused = true;
                    status.syncing = false;
                    status.transfer = None;
                });
                match commands.recv() {
                    Ok(Command::Changed) => watch.acknowledge(),
                    Ok(_) => {}
                    Err(_) => return,
                }
                continue;
            }
            reporter.update(|status| status.paused = false);

            let started = Instant::now();
            let cycle = guarded(|| run_once(&reporter, &notify, &control, &mut memory))
                .unwrap_or_else(|message| {
                    // The thread would end, and the tray would show the last state for ever.
                    log::error!("a cycle panicked: {message}");
                    for problem in reporter.set_problems(vec![format!("internal error: {message}")])
                    {
                        notify("Problem", &problem);
                    }
                    reporter.update(|status| {
                        status.syncing = false;
                        status.transfer = None;
                    });
                    Cycle {
                        interval: FALLBACK_INTERVAL,
                        folders: Vec::new(),
                        settles_at: None,
                    }
                });
            watch.set_folders(&cycle.folders);

            let mut wake = Instant::now() + cycle.interval;
            if let Some(settles_at) = cycle.settles_at {
                // The first file that was still being written can be uploaded then, and there is
                // no reason to wait for the next scan.
                let wait = Duration::from_secs(settles_at.saturating_sub(now_secs()) + 1);
                wake = wake.min(Instant::now() + wait);
            }
            let mut changed_at = None;
            loop {
                let target = changed_at.map_or(wake, |at: Instant| at.min(wake));
                let Some(remaining) = target.checked_duration_since(Instant::now()) else {
                    break;
                };
                match commands.recv_timeout(remaining) {
                    Ok(Command::SyncNow) => {
                        memory.clear();
                        drain(&commands, &watch);
                        break;
                    }
                    Ok(Command::Rescan) => {
                        drain(&commands, &watch);
                        break;
                    }
                    Ok(Command::Wake) | Err(RecvTimeoutError::Timeout) => break,
                    Ok(Command::Changed) => {
                        watch.acknowledge();
                        changed_at.get_or_insert_with(|| change_due(Instant::now(), started));
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    });
}

/// Runs the work and turns a panic in it into its message.
fn guarded<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    panic::catch_unwind(AssertUnwindSafe(work)).map_err(|payload| panic_message(&*payload))
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Drops the commands that piled up while a cycle was running, because the cycle that starts now
/// answers all of them. Several clicks on "Sync now" must not start several cycles.
fn drain(commands: &Receiver<Command>, watch: &Watch) {
    while let Ok(command) = commands.try_recv() {
        if matches!(command, Command::Changed) {
            watch.acknowledge();
        }
    }
}

/// When the cycle that a change causes starts: a moment after the change, but not sooner than a
/// minimum time after the last cycle started.
fn change_due(now: Instant, last_cycle_started: Instant) -> Instant {
    (now + CHANGE_DELAY).max(last_cycle_started + MIN_CYCLE_GAP)
}

/// What a cycle says about the next one.
struct Cycle {
    /// The time until the next scan, when nothing makes it earlier.
    interval: Duration,
    /// The folders that are watched.
    folders: Vec<PathBuf>,
    /// When the first file that was still being written can be uploaded.
    settles_at: Option<u64>,
}

/// The configuration is read again on every cycle, so editing it needs no restart.
fn run_once(
    reporter: &Reporter,
    notify: &Notifier,
    control: &Arc<Control>,
    memory: &mut Memory,
) -> Cycle {
    let config = config::config_path().and_then(|path| Config::load(&path));
    match config {
        Ok(config) => {
            let engine = Engine {
                config: &config,
                remote: &SshRemote::new(&config.ssh).with_cancel({
                    let control = Arc::clone(control);
                    Arc::new(move || control.paused())
                }),
                trash: &|path| Ok(trash::delete(path)?),
                notify: &|title, body| notify(title, body),
                stop: &|| control.paused(),
                sleep: &thread::sleep,
                reporter,
            };
            let settles_at = engine.run_cycle(memory, now_secs());
            Cycle {
                interval: Duration::from_secs(config.scan_interval_secs),
                folders: config
                    .mappings
                    .iter()
                    .filter(|mapping| mapping.enabled)
                    .map(|mapping| mapping.local.clone())
                    .collect(),
                settles_at,
            }
        }
        Err(e) => {
            // Without a readable configuration there is no setting to turn notifications off.
            for problem in reporter.set_problems(vec![format!("{e:#}")]) {
                notify("Problem", &problem);
            }
            Cycle {
                interval: FALLBACK_INTERVAL,
                folders: Vec::new(),
                settles_at: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_that_piled_up_are_answered_by_one_cycle() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let watch = Watch::new(sender.clone());
        for command in [
            Command::SyncNow,
            Command::SyncNow,
            Command::Rescan,
            Command::Wake,
        ] {
            sender.send(command).unwrap();
        }
        drain(&receiver, &watch);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn a_panic_becomes_a_message() {
        assert_eq!(guarded(|| 7), Ok(7));
        assert_eq!(guarded(|| -> u8 { panic!("broken") }), Err("broken".into()));
        let number = 3;
        assert_eq!(
            guarded(|| -> u8 { panic!("broken {number}") }),
            Err("broken 3".into())
        );
    }

    #[test]
    fn a_change_starts_a_cycle_after_a_short_delay() {
        let long_ago = Instant::now() - Duration::from_secs(3600);
        let now = Instant::now();
        assert_eq!(change_due(now, long_ago), now + CHANGE_DELAY);
    }

    #[test]
    fn changes_cannot_start_cycles_too_close_together() {
        let now = Instant::now();
        // The last cycle started a second ago.
        let started = now - Duration::from_secs(1);
        assert_eq!(change_due(now, started), started + MIN_CYCLE_GAP);
    }
}

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use crate::{
    config::{self, Config},
    ledger::Ledger,
    ssh::SshRemote,
    status::Reporter,
    sync::{Conflicts, Engine, now_secs},
};

const FALLBACK_INTERVAL: Duration = Duration::from_secs(30);

/// Asks the thread that runs the event loop to show a notification: (title, body).
pub type Notifier = Arc<dyn Fn(&str, &str) + Send + Sync>;

pub enum Command {
    /// Start the next cycle now, and compare earlier conflicts again.
    SyncNow,
    /// Look at the pause switch again.
    Wake,
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

/// Background thread: syncs one cycle every interval. "Sync now" starts the next cycle at once
/// and makes earlier conflicts be compared again. While paused it does nothing until it is woken.
pub fn spawn(
    reporter: Reporter,
    commands: Receiver<Command>,
    notify: Notifier,
    control: Arc<Control>,
) {
    thread::spawn(move || {
        let mut ledger = Ledger::default();
        let mut conflicts = Conflicts::new();
        loop {
            if control.paused() {
                reporter.update(|status| {
                    status.paused = true;
                    status.syncing = false;
                    status.transfer = None;
                });
                match commands.recv() {
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
            reporter.update(|status| status.paused = false);

            let interval = run_once(&reporter, &notify, &control, &mut ledger, &mut conflicts);
            match commands.recv_timeout(interval) {
                Ok(Command::SyncNow) => conflicts.clear(),
                Ok(Command::Wake) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// The configuration is read again on every cycle, so editing it needs no restart.
fn run_once(
    reporter: &Reporter,
    notify: &Notifier,
    control: &Control,
    ledger: &mut Ledger,
    conflicts: &mut Conflicts,
) -> Duration {
    let config = config::config_path().and_then(|path| Config::load(&path));
    match config {
        Ok(config) => {
            let engine = Engine {
                config: &config,
                remote: &SshRemote::new(&config.ssh),
                trash: &|path| Ok(trash::delete(path)?),
                notify: &|title, body| notify(title, body),
                stop: &|| control.paused(),
                sleep: &thread::sleep,
                reporter,
            };
            engine.run_cycle(ledger, conflicts, now_secs());
            Duration::from_secs(config.scan_interval_secs)
        }
        Err(e) => {
            // Without a readable configuration there is no setting to turn notifications off.
            for problem in reporter.set_problems(vec![format!("{e:#}")]) {
                notify("Problem", &problem);
            }
            FALLBACK_INTERVAL
        }
    }
}

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::{DirEntry, WalkDir};

use crate::{
    config::{Config, Mapping},
    status::{Phase, Reporter, Transfer, human_bytes, plural},
};

pub const CHUNK_SIZE: usize = 1 << 20;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
/// How long a wait lasts at most before it looks at the stop switch again.
const WAIT_SLICE: Duration = Duration::from_millis(200);
/// No retry waits longer than this.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A cycle gives up when this many files in a row fail for a reason that may be the connection.
/// Every further file would only wait through its own retries for the same failure.
const CIRCUIT_LIMIT: u32 = 2;

pub enum RemoteEntry {
    Missing,
    File {
        size: u64,
    },
    /// A directory, device file and so on: it cannot be compared as a file with the same name.
    Other,
}

/// The minimal set of remote operations. The real implementation runs the system ssh, and the
/// tests use a local directory instead.
pub trait Remote {
    fn dir_exists(&self, path: &str) -> Result<bool>;
    fn stat(&self, path: &str) -> Result<RemoteEntry>;
    fn sha256(&self, path: &str) -> Result<String>;
    /// Uploads to a temporary file and renames it only after the checksum matches. If the final
    /// name already exists it must fail instead of overwriting.
    fn upload(
        &self,
        local: &Path,
        remote: &str,
        expected_size: u64,
        control: &UploadControl,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()>;
}

/// What the caller can ask of a running upload.
pub struct UploadControl<'a> {
    /// Polled while the file is sent. When it says true the upload stops with a [`Cancelled`]
    /// error and the partial remote file stays, so a later upload resumes from it.
    pub cancel: &'a dyn Fn() -> bool,
    /// The most bytes per second to send, or None for no limit.
    pub bytes_per_sec: Option<u64>,
}

/// Sleeps long enough after sending `sent` bytes since `started` to stay within `limit` bytes per
/// second. The sleep is cut short, with [`Cancelled`], when `cancel` says true.
pub fn throttle(started: Instant, sent: u64, limit: u64, cancel: &dyn Fn() -> bool) -> Result<()> {
    let due = Duration::from_secs_f64(sent as f64 / limit as f64);
    loop {
        if cancel() {
            return Err(Cancelled.into());
        }
        let Some(left) = due.checked_sub(started.elapsed()) else {
            return Ok(());
        };
        std::thread::sleep(left.min(WAIT_SLICE));
    }
}

/// The error of an upload that was stopped on purpose, for example by pausing.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the upload was stopped")
    }
}

impl std::error::Error for Cancelled {}

/// An error that trying again cannot fix, such as a missing ssh program.
#[derive(Debug)]
pub struct Permanent(pub String);

impl std::fmt::Display for Permanent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Permanent {}

/// Whether a failure may go away by itself, like a dropped connection.
fn is_transient(error: &anyhow::Error) -> bool {
    !error.is::<Cancelled>() && !error.is::<Permanent>()
}

/// The wait before retry number `attempt` (1 for the first retry): the base delay, doubled each
/// time, up to a limit.
fn backoff(base_secs: u64, attempt: u32) -> Duration {
    let factor = 1u64 << (attempt - 1).min(16);
    Duration::from_secs(base_secs.saturating_mul(factor)).min(MAX_BACKOFF)
}

pub struct Candidate {
    pub local: PathBuf,
    /// The mapped folder that the file is in.
    pub root: PathBuf,
    pub remote: String,
    pub size: u64,
    pub modified: u64,
}

/// The result of scanning one folder.
struct Scan {
    candidates: Vec<Candidate>,
    /// Files that were modified so recently that they are probably still being written.
    waiting: usize,
    /// When the first of those has been left alone long enough, in seconds since the epoch.
    settles_at: Option<u64>,
}

pub struct Conflict {
    reason: String,
    size: u64,
    modified: u64,
}

/// Indexed by the file key (ssh destination + remote path). An unchanged file is not compared
/// again.
pub type Conflicts = BTreeMap<String, Conflict>;

enum Decision {
    Upload,
    AlreadySynced,
    Conflict(String),
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_file(path: &Path, progress: &mut dyn FnMut(u64)) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; CHUNK_SIZE];
    let mut done = 0;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(to_hex(&hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
        done += read as u64;
        progress(done);
    }
}

/// Everything one sync cycle needs besides the mutable bookkeeping.
pub struct Engine<'a> {
    pub config: &'a Config,
    pub remote: &'a dyn Remote,
    /// Moves a local file to the trash. It is a parameter so that tests never touch the real one.
    pub trash: &'a dyn Fn(&Path) -> Result<()>,
    /// Shows a system notification with a title and a body.
    pub notify: &'a dyn Fn(&str, &str),
    /// Says true when the work should stop, for example because the user paused it.
    pub stop: &'a dyn Fn() -> bool,
    /// Sleeps for a short while. It is a parameter so that tests do not have to wait.
    pub sleep: &'a dyn Fn(Duration),
    pub reporter: &'a Reporter,
}

/// What one cycle did.
#[derive(Default)]
struct Summary {
    problems: Vec<String>,
    uploaded: usize,
    uploaded_bytes: u64,
    /// When the first file that is still being written has been left alone long enough.
    settles_at: Option<u64>,
}

/// What happened to one file.
enum Handled {
    Uploaded,
    AlreadySynced,
    Conflict,
}

impl Engine<'_> {
    /// One sync cycle: check both folders, scan for new files, then compare and upload them one
    /// by one. A failing file is recorded as a problem and the rest continue; if the remote
    /// cannot be reached the whole cycle is abandoned.
    /// Returns when the first file that was still being written can be uploaded, in seconds since
    /// the epoch, so that the next cycle can be started at that moment.
    pub fn run_cycle(&self, conflicts: &mut Conflicts, now: u64) -> Option<u64> {
        self.reporter.update(|status| {
            status.syncing = true;
            status.transfer = None;
            status.queued = 0;
            status.finished = 0;
        });

        let summary = match self.cycle(conflicts, now) {
            Ok(summary) => summary,
            Err(e) => Summary {
                problems: vec![format!("{e:#}")],
                ..Summary::default()
            },
        };

        let settles_at = summary.settles_at;
        let conflict_lines: Vec<String> = conflicts
            .iter()
            .map(|(key, conflict)| format!("{} — {}", display_path(key), conflict.reason))
            .collect();
        let known_conflicts = self.reporter.snapshot().conflicts;
        let new_conflicts: Vec<String> = conflict_lines
            .iter()
            .filter(|line| !known_conflicts.contains(line))
            .cloned()
            .collect();
        let new_problems = self.reporter.set_problems(summary.problems);
        self.reporter.update(|status| {
            status.syncing = false;
            status.transfer = None;
            status.conflicts = conflict_lines;
        });

        if self.config.notifications {
            if summary.uploaded > 0 {
                let title = format!("Uploaded {}", plural(summary.uploaded, "file"));
                (self.notify)(&title, &human_bytes(summary.uploaded_bytes));
            }
            self.notify_new("conflict", &new_conflicts);
            self.notify_new("problem", &new_problems);
        }
        settles_at
    }

    /// One notification per kind, so that a burst of failures does not flood the screen.
    fn notify_new(&self, kind: &str, items: &[String]) {
        match items {
            [] => {}
            [only] => (self.notify)(&capitalized(kind), only),
            [first, ..] => (self.notify)(
                &format!("{} new {kind}s", items.len()),
                &format!("First: {first}"),
            ),
        }
    }

    fn cycle(&self, conflicts: &mut Conflicts, now: u64) -> Result<Summary> {
        let mut summary = Summary::default();
        let mut waiting = 0;

        // The folders are scanned here first. The server is only asked about a folder that has
        // something to send, so a cycle with nothing new makes no connection at all.
        let mut to_send = Vec::new();
        for mapping in self.config.mappings.iter().filter(|m| m.enabled) {
            if !mapping.local.is_dir() {
                summary.problems.push(format!(
                    "local folder does not exist: {}",
                    mapping.local.display()
                ));
                continue;
            }
            let found = match scan(mapping, self.config, now) {
                Ok(found) => found,
                Err(e) => {
                    summary.problems.push(format!(
                        "scanning {} failed: {e:#}",
                        mapping.local.display()
                    ));
                    continue;
                }
            };
            waiting += found.waiting;
            summary.settles_at = [summary.settles_at, found.settles_at]
                .into_iter()
                .flatten()
                .min();
            let fresh: Vec<_> = found
                .candidates
                .into_iter()
                .map(|candidate| (file_key(self.config, &candidate), candidate))
                .filter(|(key, c)| {
                    !conflicts
                        .get(key)
                        .is_some_and(|k| k.size == c.size && k.modified == c.modified)
                })
                .collect();
            if !fresh.is_empty() {
                to_send.push((mapping, fresh));
            }
        }

        let mut pending = Vec::new();
        for (mapping, fresh) in to_send {
            if self
                .remote
                .dir_exists(&mapping.remote)
                .context("cannot reach the remote")?
            {
                pending.extend(fresh);
            } else {
                summary
                    .problems
                    .push(format!("remote folder does not exist: {}", mapping.remote));
            }
        }
        let mut remaining_files = pending.len();
        let mut remaining_bytes: u64 = pending.iter().map(|(_, c)| c.size).sum();
        self.reporter.update(|status| {
            status.queued = remaining_files;
            status.pending_files = remaining_files;
            status.pending_bytes = remaining_bytes;
            status.waiting_files = waiting;
        });

        let mut failures_in_a_row = 0;
        for (key, candidate) in pending {
            if (self.stop)() {
                break;
            }
            let handled = match self.process_with_retries(&key, &candidate, conflicts) {
                Ok(handled) => {
                    failures_in_a_row = 0;
                    Some(handled)
                }
                // Stopped on purpose: the file stays pending and nothing is reported.
                Err(e) if e.is::<Cancelled>() => break,
                Err(e) => {
                    summary
                        .problems
                        .push(format!("{}: {e:#}", candidate.remote));
                    failures_in_a_row = if is_transient(&e) {
                        failures_in_a_row + 1
                    } else {
                        0
                    };
                    None
                }
            };
            if let Some(handled) = &handled {
                remaining_files -= 1;
                remaining_bytes -= candidate.size;
                if matches!(handled, Handled::Uploaded) {
                    summary.uploaded += 1;
                    summary.uploaded_bytes += candidate.size;
                }
            }
            let uploaded = matches!(handled, Some(Handled::Uploaded));
            self.reporter.update(|status| {
                status.finished += 1;
                status.pending_files = remaining_files;
                status.pending_bytes = remaining_bytes;
                if uploaded {
                    status.done_files += 1;
                    status.done_bytes += candidate.size;
                }
            });
            if failures_in_a_row >= CIRCUIT_LIMIT {
                summary.problems.push(
                    "stopped after repeated failures, the rest is tried in the next cycle"
                        .to_string(),
                );
                break;
            }
        }
        Ok(summary)
    }

    /// A failure that may go away by itself, such as a dropped connection, is tried again after a
    /// wait that doubles each time. The upload resumes from the partial remote file, so a retry
    /// costs little.
    fn process_with_retries(
        &self,
        key: &str,
        candidate: &Candidate,
        conflicts: &mut Conflicts,
    ) -> Result<Handled> {
        let attempts = self.config.retries + 1;
        for attempt in 1..attempts {
            match self.process(key, candidate, conflicts) {
                Err(e) if is_transient(&e) => {
                    let delay = backoff(self.config.retry_delay_secs, attempt);
                    log::warn!(
                        "{}: {e:#}; trying again in {}s (attempt {} of {attempts})",
                        candidate.remote,
                        delay.as_secs(),
                        attempt + 1
                    );
                    let name = file_name(candidate);
                    self.reporter.update(|status| {
                        status.retry = Some(format!(
                            "Trying {name} again in {}s (attempt {} of {attempts})",
                            delay.as_secs(),
                            attempt + 1
                        ));
                    });
                    let waited = self.wait(delay);
                    self.reporter.update(|status| status.retry = None);
                    waited?;
                }
                result => return result,
            }
        }
        self.process(key, candidate, conflicts)
    }

    /// Waits, and gives up with [`Cancelled`] as soon as the work is stopped.
    fn wait(&self, delay: Duration) -> Result<()> {
        let mut left = delay;
        while !left.is_zero() {
            if (self.stop)() {
                return Err(Cancelled.into());
            }
            let step = left.min(WAIT_SLICE);
            (self.sleep)(step);
            left -= step;
        }
        Ok(())
    }

    fn process(
        &self,
        key: &str,
        candidate: &Candidate,
        conflicts: &mut Conflicts,
    ) -> Result<Handled> {
        let name = file_name(candidate);
        let begin = |phase| {
            self.reporter.update(|status| {
                status.transfer = Some(Transfer::new(phase, name.clone(), candidate.size));
            });
        };

        begin(Phase::Checking);
        let decision = decide(
            self.remote,
            candidate,
            &mut progress_callback(self.reporter),
        )?;
        let handled = match decision {
            Decision::Upload => {
                begin(Phase::Uploading);
                let started = Instant::now();
                let control = UploadControl {
                    cancel: self.stop,
                    bytes_per_sec: self.config.bandwidth_limit.map(|rate| rate.0),
                };
                self.remote.upload(
                    &candidate.local,
                    &candidate.remote,
                    candidate.size,
                    &control,
                    &mut progress_callback(self.reporter),
                )?;
                log::info!(
                    "uploaded {} ({} bytes in {:.1}s)",
                    candidate.remote,
                    candidate.size,
                    started.elapsed().as_secs_f64()
                );
                Handled::Uploaded
            }
            Decision::AlreadySynced => {
                log::info!(
                    "the remote already has an identical file, skipping {}",
                    candidate.remote
                );
                Handled::AlreadySynced
            }
            Decision::Conflict(reason) => {
                log::warn!("conflict at {}: {reason}", candidate.remote);
                conflicts.insert(
                    key.to_string(),
                    Conflict {
                        reason,
                        size: candidate.size,
                        modified: candidate.modified,
                    },
                );
                return Ok(Handled::Conflict);
            }
        };

        conflicts.remove(key);
        // A file that was written to while it was sent has newer content than the remote copy,
        // and the trash would lose it. It stays, and the next cycle reports the difference.
        if changed_since_scan(candidate) {
            log::warn!(
                "{} changed while it was being sent, so it stays",
                candidate.local.display()
            );
            return Ok(handled);
        }
        // Here the remote is known to hold exactly this content, so the local copy is redundant.
        // If the trash fails, the file stays, and the next cycle finds the identical remote file
        // and only retries the trash, without uploading again.
        (self.trash)(&candidate.local)
            .context("uploaded, but cannot move the local file to the trash")?;
        log::info!("moved {} to the trash", candidate.local.display());
        remove_empty_folders(&candidate.local, &candidate.root);
        Ok(handled)
    }
}

/// Whether the file is no longer the one that was scanned. A file that cannot be read counts as
/// changed, because it must not be trashed blind.
fn changed_since_scan(candidate: &Candidate) -> bool {
    fs::metadata(&candidate.local)
        .and_then(|metadata| Ok((metadata.len(), metadata.modified()?)))
        .map_or(true, |(size, modified)| {
            let modified = modified
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            size != candidate.size || modified != candidate.modified
        })
}

/// Windows marks its own files, such as desktop.ini and Thumbs.db, and the lock files of Office
/// as hidden or system files. They are not the user's files, so neither they nor the folders that
/// are marked like that are sent.
fn is_hidden(entry: &DirEntry) -> bool {
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    entry
        .metadata()
        .is_ok_and(|metadata| metadata.file_attributes() & (HIDDEN | SYSTEM) != 0)
}

/// A folder that was dropped into a mapped folder is empty once its files went to the trash.
/// The empty folders are removed, up to but not including the mapped folder. A folder that still
/// holds anything stays, because only an empty folder can be removed.
fn remove_empty_folders(file: &Path, root: &Path) {
    let mut folder = file.parent();
    while let Some(current) = folder {
        if current == root || !current.starts_with(root) || fs::remove_dir(current).is_err() {
            break;
        }
        folder = current.parent();
    }
}

fn file_name(candidate: &Candidate) -> String {
    candidate
        .local
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// When the remote already has a file with the same name, compare sizes first and sha256 only if
/// the sizes match; only then are the files considered identical.
fn decide(
    remote: &dyn Remote,
    candidate: &Candidate,
    progress: &mut dyn FnMut(u64),
) -> Result<Decision> {
    Ok(match remote.stat(&candidate.remote)? {
        RemoteEntry::Missing => Decision::Upload,
        RemoteEntry::Other => {
            Decision::Conflict("the remote path exists but is not a regular file".to_string())
        }
        RemoteEntry::File { size } if size != candidate.size => Decision::Conflict(format!(
            "different size (local {} bytes, remote {size} bytes)",
            candidate.size
        )),
        RemoteEntry::File { .. } => {
            let local = sha256_file(&candidate.local, progress)?;
            if local == remote.sha256(&candidate.remote)? {
                Decision::AlreadySynced
            } else {
                Decision::Conflict("same size but different content".to_string())
            }
        }
    })
}

/// Large files report progress very often, so updates are throttled by time to keep the UI from
/// being flooded with refreshes.
fn progress_callback(reporter: &Reporter) -> impl FnMut(u64) {
    let mut last = Instant::now();
    move |done| {
        if last.elapsed() >= PROGRESS_INTERVAL {
            last = Instant::now();
            reporter.update(|status| {
                if let Some(transfer) = &mut status.transfer {
                    transfer.done = done;
                }
            });
        }
    }
}

fn scan(mapping: &Mapping, config: &Config, now: u64) -> Result<Scan> {
    let remote_root = mapping.remote.trim_end_matches('/');
    let ignored = mapping.ignored_extensions(config);
    let settle_secs = mapping.settle_secs(config);
    let mut candidates = Vec::new();
    let mut waiting = 0;
    let mut settles_at: Option<u64> = None;

    let walk = WalkDir::new(&mapping.local)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_hidden(entry));
    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                log::warn!("skipping an inaccessible path: {e}");
                continue;
            }
        };
        if !entry.file_type().is_file() || is_ignored(entry.path(), ignored) {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        // A file that was modified recently is most likely still being downloaded.
        if now.saturating_sub(modified) < settle_secs {
            waiting += 1;
            let at = modified + settle_secs;
            settles_at = Some(settles_at.map_or(at, |first| first.min(at)));
            continue;
        }
        let Some(relative) = relative_remote_path(&mapping.local, entry.path()) else {
            log::warn!(
                "skipping a path that is not valid UTF-8: {}",
                entry.path().display()
            );
            continue;
        };
        candidates.push(Candidate {
            local: entry.into_path(),
            root: mapping.local.clone(),
            remote: format!("{remote_root}/{relative}"),
            size: metadata.len(),
            modified,
        });
    }
    Ok(Scan {
        candidates,
        waiting,
        settles_at,
    })
}

fn is_ignored(path: &Path, ignored_extensions: &[String]) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    ignored_extensions.iter().any(|ignored| {
        ignored
            .trim_start_matches('.')
            .eq_ignore_ascii_case(extension)
    })
}

fn relative_remote_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<_>> = relative
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect();
    Some(parts?.join("/"))
}

fn file_key(config: &Config, candidate: &Candidate) -> String {
    format!("{}:{}", config.ssh.destination, candidate.remote)
}

/// A file key is "destination:path"; the UI only needs the path.
fn display_path(key: &str) -> &str {
    key.find(":/").map_or(key, |index| &key[index + 1..])
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        fs,
        sync::Arc,
    };

    use anyhow::bail;

    use super::*;
    use crate::config::Ssh;

    /// Uses a local directory as the remote: the remote path `/x/y` maps to `<root>/x/y`.
    struct FakeRemote {
        root: PathBuf,
        uploads: RefCell<Vec<String>>,
        /// The failures the next uploads run into, the last one first.
        failures: RefCell<Vec<Failure>>,
        attempts: Cell<usize>,
        /// Rewrites the local file with this content once the upload has read it.
        rewrite_after_upload: RefCell<Option<String>>,
    }

    #[derive(Clone, Copy)]
    enum Failure {
        Transient,
        Permanent,
    }

    impl FakeRemote {
        fn new(root: &Path) -> Self {
            Self {
                root: root.to_path_buf(),
                uploads: RefCell::default(),
                failures: RefCell::default(),
                attempts: Cell::new(0),
                rewrite_after_upload: RefCell::default(),
            }
        }

        fn real(&self, path: &str) -> PathBuf {
            self.root.join(path.trim_start_matches('/'))
        }
    }

    impl Remote for FakeRemote {
        fn dir_exists(&self, path: &str) -> Result<bool> {
            Ok(self.real(path).is_dir())
        }

        fn stat(&self, path: &str) -> Result<RemoteEntry> {
            let real = self.real(path);
            Ok(match fs::metadata(&real) {
                Ok(m) if m.is_file() => RemoteEntry::File { size: m.len() },
                Ok(_) => RemoteEntry::Other,
                Err(_) => RemoteEntry::Missing,
            })
        }

        fn sha256(&self, path: &str) -> Result<String> {
            sha256_file(&self.real(path), &mut |_| {})
        }

        fn upload(
            &self,
            local: &Path,
            remote: &str,
            _expected_size: u64,
            _control: &UploadControl,
            progress: &mut dyn FnMut(u64),
        ) -> Result<()> {
            self.attempts.set(self.attempts.get() + 1);
            match self.failures.borrow_mut().pop() {
                Some(Failure::Transient) => bail!("connection reset"),
                Some(Failure::Permanent) => return Err(Permanent("no ssh".into()).into()),
                None => {}
            }
            let real = self.real(remote);
            fs::create_dir_all(real.parent().unwrap())?;
            let size = fs::copy(local, real)?;
            progress(size);
            if let Some(content) = self.rewrite_after_upload.borrow_mut().take() {
                let file = fs::File::options().write(true).open(local)?;
                file.set_len(0)?;
                fs::write(local, content)?;
                file.set_modified(SystemTime::now() + Duration::from_secs(5))?;
            }
            self.uploads.borrow_mut().push(remote.to_string());
            Ok(())
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        local: PathBuf,
        remote: FakeRemote,
        config: Config,
        conflicts: Conflicts,
        reporter: Reporter,
        trashed: RefCell<Vec<PathBuf>>,
        trash_fails: Cell<bool>,
        notices: RefCell<Vec<(String, String)>>,
        stopped: Cell<bool>,
        /// Every wait that was asked for, and whether the first one stops the work.
        waits: RefCell<Vec<Duration>>,
        stop_on_wait: Cell<bool>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let local = dir.path().join("local");
            let remote_root = dir.path().join("remote");
            fs::create_dir_all(&local).unwrap();
            fs::create_dir_all(remote_root.join("videos")).unwrap();
            Self {
                local: local.clone(),
                remote: FakeRemote::new(&remote_root),
                config: Config {
                    ssh: Ssh {
                        destination: "vps".into(),
                        port: None,
                        identity_file: None,
                    },
                    mappings: vec![Mapping::new(local, "/videos")],
                    scan_interval_secs: 30,
                    settle_secs: 0,
                    ignored_extensions: vec!["part".into()],
                    notifications: true,
                    retries: 3,
                    retry_delay_secs: 2,
                    bandwidth_limit: None,
                },
                conflicts: Conflicts::new(),
                reporter: Reporter::new(Arc::new(|| {})),
                trashed: RefCell::default(),
                trash_fails: Cell::new(false),
                notices: RefCell::default(),
                stopped: Cell::new(false),
                waits: RefCell::default(),
                stop_on_wait: Cell::new(false),
                _dir: dir,
            }
        }

        fn run(&mut self) -> Option<u64> {
            let trash = |path: &Path| -> Result<()> {
                if self.trash_fails.get() {
                    bail!("the trash is unavailable");
                }
                fs::remove_file(path)?;
                self.trashed.borrow_mut().push(path.to_path_buf());
                Ok(())
            };
            Engine {
                config: &self.config,
                remote: &self.remote,
                trash: &trash,
                notify: &|title, body| {
                    self.notices
                        .borrow_mut()
                        .push((title.to_string(), body.to_string()));
                },
                stop: &|| self.stopped.get(),
                sleep: &|step| {
                    self.waits.borrow_mut().push(step);
                    if self.stop_on_wait.get() {
                        self.stopped.set(true);
                    }
                },
                reporter: &self.reporter,
            }
            .run_cycle(&mut self.conflicts, now_secs())
        }

        fn waited(&self) -> Duration {
            self.waits.borrow().iter().sum()
        }

        fn titles(&self) -> Vec<String> {
            self.notices
                .borrow()
                .iter()
                .map(|(title, _)| title.clone())
                .collect()
        }

        fn uploads(&self) -> Vec<String> {
            self.remote.uploads.borrow().clone()
        }
    }

    #[test]
    fn uploads_new_files_once_and_mirrors_subdirectories() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fs::create_dir(fx.local.join("show")).unwrap();
        fs::write(fx.local.join("show").join("b.mkv"), "bbb").unwrap();

        fx.run();
        assert_eq!(fx.uploads(), ["/videos/a.mkv", "/videos/show/b.mkv"]);
        assert_eq!(
            fs::read_to_string(fx.remote.real("/videos/show/b.mkv")).unwrap(),
            "bbb"
        );
        assert!(fx.reporter.snapshot().problems.is_empty());

        fx.run();
        assert_eq!(
            fx.uploads().len(),
            2,
            "the second cycle must not upload again"
        );
    }

    #[test]
    fn identical_remote_file_is_skipped() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "same").unwrap();
        fs::write(fx.remote.real("/videos/a.mkv"), "same").unwrap();

        fx.run();
        assert!(fx.uploads().is_empty());
        assert!(fx.conflicts.is_empty());
        assert!(fx.reporter.snapshot().problems.is_empty());
    }

    #[test]
    fn different_size_or_content_is_a_conflict_and_never_overwritten() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("size.mkv"), "local-longer").unwrap();
        fs::write(fx.remote.real("/videos/size.mkv"), "short").unwrap();
        fs::write(fx.local.join("content.mkv"), "aaaa").unwrap();
        fs::write(fx.remote.real("/videos/content.mkv"), "bbbb").unwrap();

        fx.run();
        assert!(fx.uploads().is_empty());
        assert_eq!(fx.conflicts.len(), 2);
        assert_eq!(
            fs::read_to_string(fx.remote.real("/videos/size.mkv")).unwrap(),
            "short"
        );
        assert_eq!(fx.reporter.snapshot().conflicts.len(), 2);

        // An unchanged file is not compared again; once the conflicts are cleared it is compared
        // and reported again.
        fx.run();
        assert_eq!(fx.conflicts.len(), 2);
        fx.conflicts.clear();
        fx.run();
        assert_eq!(fx.conflicts.len(), 2);
    }

    #[test]
    fn a_file_written_during_the_upload_is_not_trashed() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "old").unwrap();
        *fx.remote.rewrite_after_upload.borrow_mut() = Some("new".into());

        fx.run();
        assert!(fx.trashed.borrow().is_empty());
        assert_eq!(fs::read_to_string(fx.local.join("a.mkv")).unwrap(), "new");

        // The next cycle sees different content on the remote and reports it, without overwriting.
        fx.run();
        assert_eq!(fx.conflicts.len(), 1);
        assert_eq!(
            fs::read_to_string(fx.remote.real("/videos/a.mkv")).unwrap(),
            "old"
        );
    }

    #[test]
    fn remote_directory_with_same_name_is_a_conflict() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "x").unwrap();
        fs::create_dir(fx.remote.real("/videos/a.mkv")).unwrap();

        fx.run();
        assert!(fx.uploads().is_empty());
        assert_eq!(fx.conflicts.len(), 1);
    }

    #[test]
    fn missing_directories_are_reported_and_nothing_is_uploaded() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "x").unwrap();
        fx.config.mappings[0].remote = "/nope".into();
        fx.config
            .mappings
            .push(Mapping::new(fx.local.join("missing"), "/videos"));

        fx.run();
        assert!(fx.uploads().is_empty());
        let problems = fx.reporter.snapshot().problems;
        assert_eq!(problems.len(), 2);
        assert!(problems[0].contains("local folder does not exist"));
        assert!(problems[1].contains("remote folder does not exist"));
    }

    #[test]
    fn a_cycle_with_nothing_new_does_not_ask_the_server() {
        let mut fx = Fixture::new();
        // The server would say that the folder is missing, but there is nothing to send.
        fx.config.mappings[0].remote = "/nope".into();
        fx.run();
        assert!(fx.reporter.snapshot().problems.is_empty());
    }

    #[test]
    fn skips_ignored_extensions_and_unsettled_files() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("movie.mkv.PART"), "x").unwrap();
        fx.run();
        assert!(fx.uploads().is_empty());

        fx.config.settle_secs = 3600;
        fs::write(fx.local.join("fresh.mkv"), "x").unwrap();
        let settles_at = fx.run();
        assert!(fx.uploads().is_empty());
        // The file can be uploaded an hour after it was written, and the cycle says so.
        let settles_at = settles_at.expect("a file is still being written");
        assert!((now_secs() + 3590..=now_secs() + 3600).contains(&settles_at));
    }

    #[test]
    fn nothing_is_waiting_when_all_files_have_settled() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "x").unwrap();
        assert_eq!(fx.run(), None);
    }

    #[test]
    fn changed_file_is_checked_again() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "v1").unwrap();
        fx.run();
        assert_eq!(fx.uploads().len(), 1);

        // The local content changed while the remote has the old version: never overwrite silently.
        fs::write(fx.local.join("a.mkv"), "version-two").unwrap();
        fx.run();
        assert_eq!(fx.uploads().len(), 1);
        assert_eq!(fx.conflicts.len(), 1);
    }

    #[test]
    fn hidden_and_system_files_are_not_sent() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "a").unwrap();
        fs::write(fx.local.join("desktop.ini"), "[.ShellClassInfo]").unwrap();
        fs::create_dir(fx.local.join("hidden")).unwrap();
        fs::write(fx.local.join("hidden/b.mkv"), "b").unwrap();
        for (path, flags) in [("desktop.ini", "+h +s"), ("hidden", "+h")] {
            let mut command = std::process::Command::new("attrib");
            command.args(flags.split(' ')).arg(fx.local.join(path));
            assert!(command.status().unwrap().success());
        }

        fx.run();
        assert_eq!(fx.uploads(), ["/videos/a.mkv"]);
    }

    #[test]
    fn a_dropped_folder_goes_away_once_its_files_are_in_the_trash() {
        let mut fx = Fixture::new();
        fs::create_dir_all(fx.local.join("show/season 1")).unwrap();
        fs::create_dir_all(fx.local.join("keep")).unwrap();
        fs::write(fx.local.join("show/season 1/e01.mkv"), "1").unwrap();
        fs::write(fx.local.join("show/season 1/e02.mkv"), "2").unwrap();
        fs::write(fx.local.join("keep/note.mkv"), "n").unwrap();
        fs::write(fx.local.join("keep/still.txt.part"), "p").unwrap();

        fx.run();
        assert_eq!(fx.uploads().len(), 3);
        assert!(
            !fx.local.join("show").exists(),
            "the emptied folders are gone"
        );
        assert!(fx.local.join("keep").exists(), "a folder with a file stays");
        assert!(fx.local.exists(), "the mapped folder itself stays");
    }

    #[test]
    fn trashes_verified_files_but_never_conflicting_ones() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("new.mkv"), "new").unwrap();
        fs::write(fx.local.join("same.mkv"), "same").unwrap();
        fs::write(fx.remote.real("/videos/same.mkv"), "same").unwrap();
        fs::write(fx.local.join("clash.mkv"), "mine").unwrap();
        fs::write(fx.remote.real("/videos/clash.mkv"), "yours").unwrap();

        fx.run();
        assert_eq!(fx.uploads(), ["/videos/new.mkv"]);
        assert!(
            !fx.local.join("new.mkv").exists(),
            "uploaded files are trashed"
        );
        assert!(
            !fx.local.join("same.mkv").exists(),
            "identical files are trashed"
        );
        assert!(
            fx.local.join("clash.mkv").exists(),
            "conflicts are never touched"
        );
        assert_eq!(fx.trashed.borrow().len(), 2);
        assert_eq!(fx.conflicts.len(), 1);
        assert!(fx.reporter.snapshot().problems.is_empty());
    }

    #[test]
    fn a_failing_trash_is_retried_without_uploading_again() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fx.trash_fails.set(true);

        fx.run();
        assert_eq!(fx.uploads().len(), 1);
        assert!(fx.local.join("a.mkv").exists());
        let problems = fx.reporter.snapshot().problems;
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("trash"), "{problems:?}");

        fx.trash_fails.set(false);
        fx.run();
        assert_eq!(fx.uploads().len(), 1, "the file must not be uploaded twice");
        assert!(!fx.local.join("a.mkv").exists());
        assert!(fx.reporter.snapshot().problems.is_empty());
    }

    #[test]
    fn one_notification_summarizes_the_uploads_of_a_cycle() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fs::write(fx.local.join("b.mkv"), "bbbb").unwrap();

        fx.run();
        assert_eq!(fx.titles(), ["Uploaded 2 files"]);
        assert_eq!(fx.notices.borrow()[0].1, "7 B");

        fx.run();
        assert_eq!(fx.titles().len(), 1, "an idle cycle says nothing");
    }

    #[test]
    fn a_conflict_is_announced_once() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "mine").unwrap();
        fs::write(fx.remote.real("/videos/a.mkv"), "yours-longer").unwrap();

        fx.run();
        assert_eq!(fx.titles(), ["Conflict"]);
        assert!(fx.notices.borrow()[0].1.contains("/videos/a.mkv"));

        fx.run();
        assert_eq!(
            fx.titles().len(),
            1,
            "the same conflict is not announced again"
        );
    }

    #[test]
    fn many_new_problems_give_one_notification() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "x").unwrap();
        fx.config.mappings[0].remote = "/nope".into();
        fx.config
            .mappings
            .push(Mapping::new(fx.local.join("missing"), "/videos"));

        fx.run();
        assert_eq!(fx.titles(), ["2 new problems"]);

        fx.run();
        assert_eq!(
            fx.titles().len(),
            1,
            "the same problems are not announced again"
        );
    }

    #[test]
    fn notifications_can_be_turned_off() {
        let mut fx = Fixture::new();
        fx.config.notifications = false;
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fx.config
            .mappings
            .push(Mapping::new(fx.local.join("missing"), "/videos"));

        fx.run();
        assert_eq!(fx.uploads().len(), 1);
        assert!(fx.notices.borrow().is_empty());
    }

    #[test]
    fn pluralizes_and_capitalizes() {
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(3, "file"), "3 files");
        assert_eq!(capitalized("problem"), "Problem");
    }

    #[test]
    fn counts_pending_waiting_and_finished_files() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fs::write(fx.local.join("b.mkv"), "bbbb").unwrap();

        // Files that were just written are waited for, not uploaded.
        fx.config.settle_secs = 3600;
        fx.run();
        let status = fx.reporter.snapshot();
        assert_eq!(status.waiting_files, 2);
        assert_eq!((status.pending_files, status.done_files), (0, 0));

        fx.config.settle_secs = 0;
        fx.run();
        let status = fx.reporter.snapshot();
        assert_eq!(status.waiting_files, 0);
        assert_eq!((status.pending_files, status.pending_bytes), (0, 0));
        assert_eq!((status.done_files, status.done_bytes), (2, 7));
    }

    #[test]
    fn a_stopped_cycle_leaves_the_files_pending_and_reports_no_problem() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fs::write(fx.local.join("b.mkv"), "bbbb").unwrap();
        fx.stopped.set(true);

        fx.run();
        assert!(fx.uploads().is_empty());
        let status = fx.reporter.snapshot();
        assert_eq!((status.pending_files, status.pending_bytes), (2, 7));
        assert!(status.problems.is_empty());
        assert!(fx.notices.borrow().is_empty());

        fx.stopped.set(false);
        fx.run();
        assert_eq!(fx.uploads().len(), 2);
        assert_eq!(fx.reporter.snapshot().pending_files, 0);
    }

    #[test]
    fn a_failing_file_stays_pending() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fx.trash_fails.set(true);

        fx.run();
        let status = fx.reporter.snapshot();
        assert_eq!((status.pending_files, status.pending_bytes), (1, 3));
        assert_eq!(status.problems.len(), 1);
    }

    #[test]
    fn a_disabled_mapping_is_ignored_and_a_mapping_overrides_the_settings() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fx.config.mappings[0].enabled = false;
        fx.run();
        assert!(fx.uploads().is_empty());
        assert!(fx.reporter.snapshot().problems.is_empty());

        fx.config.mappings[0].enabled = true;
        fx.config.mappings[0].ignored_extensions = Some(vec!["skip".into()]);
        fs::write(fx.local.join("b.skip"), "x").unwrap();
        fx.run();
        assert_eq!(fx.uploads(), ["/videos/a.mkv"]);
        assert!(
            !fx.local.join("a.mkv").exists(),
            "the sent file is in the trash"
        );
        assert!(
            fx.local.join("b.skip").exists(),
            "the mapping ignores .skip"
        );
    }

    fn fail(fx: &Fixture, failures: &[Failure]) {
        *fx.remote.failures.borrow_mut() = failures.iter().rev().copied().collect();
    }

    #[test]
    fn a_transient_failure_is_tried_again_after_a_growing_wait() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fail(&fx, &[Failure::Transient, Failure::Transient]);

        fx.run();
        assert_eq!(fx.remote.attempts.get(), 3);
        assert_eq!(fx.uploads(), ["/videos/a.mkv"]);
        assert!(fx.reporter.snapshot().problems.is_empty());
        assert_eq!(fx.waited(), Duration::from_secs(2 + 4));
        assert_eq!(
            fx.titles(),
            ["Uploaded 1 file"],
            "the retries are not announced"
        );
        assert!(fx.reporter.snapshot().retry.is_none());
    }

    #[test]
    fn the_last_retry_gives_up_and_reports_the_problem() {
        let mut fx = Fixture::new();
        fx.config.retries = 2;
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fail(&fx, &[Failure::Transient; 5]);

        fx.run();
        assert_eq!(fx.remote.attempts.get(), 3, "one try and two retries");
        assert!(fx.uploads().is_empty());
        let status = fx.reporter.snapshot();
        assert_eq!(status.problems.len(), 1);
        assert!(status.problems[0].contains("connection reset"));
        assert_eq!((status.pending_files, status.pending_bytes), (1, 3));
        assert_eq!(fx.waited(), Duration::from_secs(2 + 4));
    }

    #[test]
    fn a_failure_that_cannot_go_away_is_not_tried_again() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fail(&fx, &[Failure::Permanent]);

        fx.run();
        assert_eq!(fx.remote.attempts.get(), 1);
        assert!(fx.waits.borrow().is_empty());
        assert_eq!(fx.reporter.snapshot().problems.len(), 1);
    }

    #[test]
    fn repeated_failures_end_the_cycle() {
        let mut fx = Fixture::new();
        for name in ["a", "b", "c", "d"] {
            fs::write(fx.local.join(format!("{name}.mkv")), "x").unwrap();
        }
        fail(&fx, &[Failure::Transient; 40]);

        fx.run();
        // Two files used up their tries, and the other two were not even started.
        assert_eq!(fx.remote.attempts.get(), 8);
        let problems = fx.reporter.snapshot().problems;
        assert_eq!(problems.len(), 3);
        assert!(problems[2].contains("stopped after repeated failures"));

        // The next cycle starts again with every file.
        fail(&fx, &[]);
        fx.run();
        assert_eq!(fx.uploads().len(), 4);
    }

    #[test]
    fn only_failures_that_may_be_the_connection_end_the_cycle() {
        let mut fx = Fixture::new();
        for name in ["a", "b", "c"] {
            fs::write(fx.local.join(format!("{name}.mkv")), "x").unwrap();
        }
        fx.config.retries = 0;
        fail(&fx, &[Failure::Transient, Failure::Transient]);
        // With retries off, a and b are already the limit.
        fx.run();
        assert_eq!(fx.remote.attempts.get(), 2);
        // A permanent failure is about the file, not the connection.
        fail(
            &fx,
            &[Failure::Permanent, Failure::Permanent, Failure::Permanent],
        );
        fx.run();
        assert_eq!(fx.remote.attempts.get(), 5);
    }

    #[test]
    fn retries_can_be_turned_off() {
        let mut fx = Fixture::new();
        fx.config.retries = 0;
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fail(&fx, &[Failure::Transient]);

        fx.run();
        assert_eq!(fx.remote.attempts.get(), 1);
        assert_eq!(fx.reporter.snapshot().problems.len(), 1);
    }

    #[test]
    fn pausing_during_the_wait_stops_without_a_problem() {
        let mut fx = Fixture::new();
        fs::write(fx.local.join("a.mkv"), "aaa").unwrap();
        fail(&fx, &[Failure::Transient]);
        fx.stop_on_wait.set(true);

        fx.run();
        assert_eq!(fx.remote.attempts.get(), 1, "no second try");
        let status = fx.reporter.snapshot();
        assert!(status.problems.is_empty());
        assert_eq!(status.pending_files, 1);
        assert!(status.retry.is_none());
    }

    #[test]
    fn the_wait_doubles_up_to_a_limit() {
        assert_eq!(backoff(2, 1), Duration::from_secs(2));
        assert_eq!(backoff(2, 2), Duration::from_secs(4));
        assert_eq!(backoff(2, 3), Duration::from_secs(8));
        assert_eq!(backoff(2, 10), Duration::from_secs(60));
        assert_eq!(backoff(0, 3), Duration::ZERO);
    }
}

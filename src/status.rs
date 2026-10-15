use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Instant,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The remote already has a file with this name, so the local file is being hashed to compare.
    Checking,
    Uploading,
}

#[derive(Clone)]
pub struct Transfer {
    pub phase: Phase,
    pub file: String,
    pub done: u64,
    pub total: u64,
    pub started: Instant,
}

impl Transfer {
    pub fn new(phase: Phase, file: String, total: u64) -> Self {
        Self {
            phase,
            file,
            done: 0,
            total,
            started: Instant::now(),
        }
    }

    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32
    }

    fn bytes_per_sec(&self) -> Option<f64> {
        let elapsed = self.started.elapsed().as_secs_f64();
        (elapsed >= 1.0).then(|| self.done as f64 / elapsed)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Idle,
    Busy,
    Warning,
    Error,
    Paused,
}

#[derive(Clone, Default)]
pub struct Status {
    pub syncing: bool,
    pub paused: bool,
    pub transfer: Option<Transfer>,
    /// Files to handle in this cycle, and how many of them are done.
    pub queued: usize,
    pub finished: usize,
    /// Files that are known to need an upload and have not been handled yet, or failed and will
    /// be tried again.
    pub pending_files: usize,
    pub pending_bytes: u64,
    /// Files that were modified so recently that they are probably still being written.
    pub waiting_files: usize,
    /// Uploaded since the program started.
    pub done_files: usize,
    pub done_bytes: u64,
    pub conflicts: Vec<String>,
    pub problems: Vec<String>,
    /// Set while a failed upload waits for its next try.
    pub retry: Option<String>,
}

impl Status {
    pub fn icon_state(&self) -> IconState {
        if !self.problems.is_empty() {
            IconState::Error
        } else if self.paused {
            IconState::Paused
        } else if self.syncing {
            IconState::Busy
        } else if !self.conflicts.is_empty() {
            IconState::Warning
        } else {
            IconState::Idle
        }
    }

    /// The two rows at the top of the menu: what is going on, and the details of it. While a file
    /// is being sent the details are its progress, otherwise they say what is pending and what was
    /// uploaded.
    pub fn headline(&self) -> [String; 2] {
        let details = if self.transfer.is_some() {
            self.progress_line()
        } else {
            self.counts_line()
        };
        [self.state_line(), details]
    }

    /// The title of the menu that lists the conflicts and the problems.
    pub fn attention_title(&self) -> String {
        let mut parts = Vec::new();
        if !self.problems.is_empty() {
            parts.push(plural(self.problems.len(), "problem"));
        }
        if !self.conflicts.is_empty() {
            parts.push(plural(self.conflicts.len(), "conflict"));
        }
        if parts.is_empty() {
            "No conflicts or problems".to_string()
        } else {
            parts.join(" · ")
        }
    }

    pub fn needs_attention(&self) -> bool {
        !self.problems.is_empty() || !self.conflicts.is_empty()
    }

    pub fn tooltip(&self) -> String {
        let [state, details] = self.headline();
        let mut text = state;
        if self.transfer.is_some() || self.pending_files > 0 {
            text = format!("{text}\n{details}");
        }
        if self.needs_attention() {
            text = format!("{text}\n{}", self.attention_title());
        }
        // Windows limits tray tooltips to 127 characters.
        text.chars().take(120).collect()
    }

    fn state_line(&self) -> String {
        let position = format!(
            "{} of {}",
            (self.finished + 1).min(self.queued),
            self.queued
        );
        let file = |t: &Transfer| shorten(&t.file, MAX_FILE_NAME);
        match &self.transfer {
            Some(t) if t.phase == Phase::Uploading => {
                format!("Uploading {position} · {}", file(t))
            }
            Some(t) => format!("Comparing {position} · {}", file(t)),
            None if self.retry.is_some() => self.retry.clone().unwrap_or_default(),
            None if self.paused => "Paused".to_string(),
            None if self.syncing => "Scanning…".to_string(),
            None => "Idle".to_string(),
        }
    }

    fn progress_line(&self) -> String {
        let Some(t) = &self.transfer else {
            return "—".to_string();
        };
        let mut line = format!(
            "{:.0}% · {} of {}",
            t.fraction() * 100.0,
            human_bytes(t.done),
            human_bytes(t.total),
        );
        if let Some(speed) = t.bytes_per_sec() {
            line.push_str(&format!(" · {}/s", human_bytes(speed as u64)));
        }
        line
    }

    /// What is waiting and what is done, on one row.
    fn counts_line(&self) -> String {
        let mut parts = Vec::new();
        if self.pending_files == 0 {
            parts.push("Nothing pending".to_string());
        } else {
            parts.push(format!(
                "{} pending ({})",
                self.pending_files,
                human_bytes(self.pending_bytes)
            ));
        }
        if self.waiting_files > 0 {
            parts.push(format!("{} still being written", self.waiting_files));
        }
        if self.done_files > 0 {
            parts.push(format!(
                "{} uploaded ({})",
                self.done_files,
                human_bytes(self.done_bytes)
            ));
        }
        parts.join(" · ")
    }
}

/// The longest file name shown in a menu row.
const MAX_FILE_NAME: usize = 36;

/// Cuts a long text down to `max` characters by leaving out the middle, so that both the start and
/// the end, which is where a file name has its extension, stay readable.
pub fn shorten(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_string();
    }
    let kept = max.saturating_sub(1);
    let head = kept * 2 / 3;
    let tail = kept - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(length - tail).collect();
    format!("{start}…{end}")
}

pub fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// State written by the worker thread and read by the UI thread. Every update wakes the UI.
#[derive(Clone)]
pub struct Reporter {
    status: Arc<Mutex<Status>>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl Reporter {
    pub fn new(notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            status: Arc::default(),
            notify,
        }
    }

    pub fn update(&self, change: impl FnOnce(&mut Status)) {
        change(&mut self.status.lock().unwrap_or_else(PoisonError::into_inner));
        (self.notify)();
    }

    pub fn snapshot(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the problem list and returns the problems that were not there before. A problem
    /// that persists is logged once, so a broken connection does not flood the log on every
    /// scan.
    pub fn set_problems(&self, problems: Vec<String>) -> Vec<String> {
        let previous = self.snapshot().problems;
        let new: Vec<String> = problems
            .iter()
            .filter(|p| !previous.contains(p))
            .cloned()
            .collect();
        for problem in &new {
            tracing::error!("{problem}");
        }
        self.update(|status| status.problems = problems);
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn pluralizes() {
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(3, "file"), "3 files");
    }

    #[test]
    fn icon_state_priority() {
        let mut status = Status::default();
        assert!(status.icon_state() == IconState::Idle);
        status.conflicts.push("x".into());
        assert!(status.icon_state() == IconState::Warning);
        status.syncing = true;
        assert!(status.icon_state() == IconState::Busy);
        status.paused = true;
        assert!(status.icon_state() == IconState::Paused);
        status.problems.push("y".into());
        assert!(status.icon_state() == IconState::Error);
    }

    #[test]
    fn describes_what_is_pending_and_what_is_done() {
        let mut status = Status::default();
        assert_eq!(status.counts_line(), "Nothing pending");

        status.pending_files = 3;
        status.pending_bytes = 3 * 1024 * 1024;
        status.waiting_files = 1;
        status.done_files = 1;
        status.done_bytes = 512;
        assert_eq!(
            status.counts_line(),
            "3 pending (3.0 MiB) · 1 still being written · 1 uploaded (512 B)"
        );
        status.pending_files = 0;
        status.waiting_files = 0;
        assert_eq!(status.counts_line(), "Nothing pending · 1 uploaded (512 B)");
    }

    #[test]
    fn the_headline_shows_the_progress_only_while_sending() {
        let mut status = Status::default();
        assert_eq!(status.headline(), ["Idle", "Nothing pending"]);

        let mut transfer = Transfer::new(Phase::Uploading, "a.mkv".into(), 200);
        transfer.done = 100;
        status.transfer = Some(transfer);
        status.queued = 5;
        status.finished = 1;
        let [state, details] = status.headline();
        assert_eq!(state, "Uploading 2 of 5 · a.mkv");
        assert!(details.contains("50% · 100 B of 200 B"), "{details}");
    }

    #[test]
    fn the_attention_title_counts_problems_and_conflicts() {
        let mut status = Status::default();
        assert_eq!(status.attention_title(), "No conflicts or problems");
        assert!(!status.needs_attention());

        status.problems = vec!["a".into()];
        assert_eq!(status.attention_title(), "1 problem");
        status.conflicts = vec!["b".into(), "c".into()];
        assert_eq!(status.attention_title(), "1 problem · 2 conflicts");
        status.problems.clear();
        assert_eq!(status.attention_title(), "2 conflicts");
        assert!(status.needs_attention());
    }

    #[test]
    fn long_texts_lose_their_middle() {
        assert_eq!(shorten("short.mkv", 36), "short.mkv");
        let long = "Season 02 - Episode 05 - The Long Title Of A Video File.mkv";
        let short = shorten(long, 36);
        assert_eq!(short.chars().count(), 36);
        assert!(short.starts_with("Season 02"));
        assert!(short.ends_with("File.mkv"));
        assert!(short.contains('…'));
        // Characters that take more than one byte are cut at character boundaries.
        assert_eq!(shorten("ééééééééééé", 5).chars().count(), 5);
    }

    #[test]
    fn shows_a_retry_while_waiting_for_the_next_try() {
        let status = Status {
            retry: Some("Trying a.mkv again in 4s (attempt 3 of 4)".into()),
            ..Status::default()
        };
        assert_eq!(
            status.state_line(),
            "Trying a.mkv again in 4s (attempt 3 of 4)"
        );
    }

    #[test]
    fn says_paused_when_nothing_is_transferring() {
        let status = Status {
            paused: true,
            ..Status::default()
        };
        assert_eq!(status.state_line(), "Paused");
    }

    #[test]
    fn tooltip_fits_windows_limit() {
        let status = Status {
            problems: vec!["very long ".repeat(50)],
            ..Status::default()
        };
        assert!(status.tooltip().chars().count() <= 127);
    }
}

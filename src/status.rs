use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
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
    /// When and where the speed was last measured.
    sample: (Instant, u64),
    speed: Option<f64>,
}

/// The speed is measured over windows of this length. A resumed upload starts with the bytes it
/// has skipped, so the speed since the start would be too high.
const SPEED_WINDOW: Duration = Duration::from_secs(3);

impl Transfer {
    pub fn new(phase: Phase, file: String, total: u64) -> Self {
        Self {
            phase,
            file,
            done: 0,
            total,
            sample: (Instant::now(), 0),
            speed: None,
        }
    }

    /// Records how far the transfer is, and measures the speed once a window has passed.
    pub fn advance(&mut self, done: u64) {
        self.advance_at(Instant::now(), done);
    }

    fn advance_at(&mut self, now: Instant, done: u64) {
        self.done = done;
        let (since, before) = self.sample;
        let elapsed = now.saturating_duration_since(since);
        if elapsed >= SPEED_WINDOW {
            self.speed = Some(done.saturating_sub(before) as f64 / elapsed.as_secs_f64());
            self.sample = (now, done);
        }
    }

    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32
    }

    fn bytes_per_sec(&self) -> Option<f64> {
        self.speed
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

/// A conflict as the menu lists it: what to say, and the local file it is about.
#[derive(Clone, PartialEq, Eq)]
pub struct ConflictLine {
    pub text: String,
    pub local: PathBuf,
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
    pub conflicts: Vec<ConflictLine>,
    pub problems: Vec<String>,
    /// Set while a failed upload waits for its next try.
    pub retry: Option<String>,
}

impl Status {
    pub fn icon_state(&self) -> IconState {
        if self.paused {
            IconState::Paused
        } else if !self.problems.is_empty() {
            IconState::Error
        } else if self.syncing {
            IconState::Busy
        } else if !self.conflicts.is_empty() {
            IconState::Warning
        } else {
            IconState::Idle
        }
    }

    /// The rows at the top of the menu: what is going on, and, when there is something to say, the
    /// details of it. While a file is being sent the details are its progress, otherwise they are
    /// what is pending and what was uploaded.
    pub fn headline(&self) -> (String, Option<String>) {
        let details = if self.transfer.is_some() {
            Some(self.progress_line())
        } else {
            self.counts_line()
        };
        (self.state_line(), details)
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
            "Problems".to_string()
        } else {
            parts.join(" · ")
        }
    }

    pub fn needs_attention(&self) -> bool {
        !self.problems.is_empty() || !self.conflicts.is_empty()
    }

    pub fn tooltip(&self) -> String {
        let (mut text, details) = self.headline();
        if let Some(details) = details {
            text = format!("{text}\n{details}");
        }
        if self.needs_attention() {
            text = format!("{text}\n{}", self.attention_title());
        }
        truncate_utf16(&text, MAX_TOOLTIP_UNITS)
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

    /// What is waiting and what is done, on one row. None when there is nothing to say.
    fn counts_line(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.pending_files > 0 {
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
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// Windows limits tray tooltips to 127 UTF-16 units. A character outside the basic plane, such
/// as an emoji in a file name, takes two of them.
const MAX_TOOLTIP_UNITS: usize = 120;

/// Cuts the text so that it takes at most `max` UTF-16 units, never in the middle of a character.
fn truncate_utf16(text: &str, max: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect()
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
            log::error!("{problem}");
        }
        self.update(|status| status.problems = problems);
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict(text: &str) -> ConflictLine {
        ConflictLine {
            text: text.into(),
            local: PathBuf::from(text),
        }
    }

    #[test]
    fn the_speed_is_measured_over_windows() {
        let mut transfer = Transfer::new(Phase::Uploading, "a".into(), 1_000_000);
        let start = transfer.sample.0;
        let second = Duration::from_secs(1);

        transfer.advance_at(start + second, 1_000);
        assert_eq!(transfer.bytes_per_sec(), None, "no window has passed yet");
        transfer.advance_at(start + 3 * second, 6_000);
        assert_eq!(transfer.bytes_per_sec(), Some(2_000.0));
        transfer.advance_at(start + 4 * second, 7_000);
        assert_eq!(transfer.done, 7_000);
        assert_eq!(transfer.bytes_per_sec(), Some(2_000.0), "same window");
        // The speed follows the last window, not the average since the start.
        transfer.advance_at(start + 6 * second, 18_000);
        assert_eq!(transfer.bytes_per_sec(), Some(4_000.0));
    }

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
        status.conflicts.push(conflict("x"));
        assert!(status.icon_state() == IconState::Warning);
        status.syncing = true;
        assert!(status.icon_state() == IconState::Busy);
        status.problems.push("y".into());
        assert!(status.icon_state() == IconState::Error);
        // The pause is the user's choice and the icon agrees with the "Paused" in the menu. The
        // problems stay listed in the menu.
        status.paused = true;
        assert!(status.icon_state() == IconState::Paused);
    }

    #[test]
    fn describes_what_is_pending_and_what_is_done() {
        let mut status = Status::default();
        assert_eq!(status.counts_line(), None);

        status.pending_files = 3;
        status.pending_bytes = 3 * 1024 * 1024;
        status.waiting_files = 1;
        status.done_files = 1;
        status.done_bytes = 512;
        assert_eq!(
            status.counts_line().as_deref(),
            Some("3 pending (3.0 MiB) · 1 still being written · 1 uploaded (512 B)")
        );
        status.pending_files = 0;
        status.waiting_files = 0;
        assert_eq!(status.counts_line().as_deref(), Some("1 uploaded (512 B)"));
    }

    #[test]
    fn the_headline_shows_the_progress_only_while_sending() {
        let mut status = Status::default();
        assert_eq!(status.headline(), ("Idle".to_string(), None));

        let mut transfer = Transfer::new(Phase::Uploading, "a.mkv".into(), 200);
        transfer.done = 100;
        status.transfer = Some(transfer);
        status.queued = 5;
        status.finished = 1;
        let (state, details) = status.headline();
        assert_eq!(state, "Uploading 2 of 5 · a.mkv");
        let details = details.unwrap();
        assert!(details.contains("50% · 100 B of 200 B"), "{details}");
    }

    #[test]
    fn the_attention_title_counts_problems_and_conflicts() {
        let mut status = Status::default();
        assert_eq!(status.attention_title(), "Problems");
        assert!(!status.needs_attention());

        status.problems = vec!["a".into()];
        assert_eq!(status.attention_title(), "1 problem");
        status.conflicts = vec![conflict("b"), conflict("c")];
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

    #[test]
    fn tooltip_counts_utf16_units() {
        let status = Status {
            retry: Some(format!("Trying {} again in 4s", "😀".repeat(100))),
            ..Status::default()
        };
        let tooltip = status.tooltip();
        assert!(tooltip.encode_utf16().count() <= 127, "{tooltip}");
        assert_eq!(truncate_utf16("a😀b", 2), "a");
        assert_eq!(truncate_utf16("a😀b", 3), "a😀");
    }
}

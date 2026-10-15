use crate::notifier;

/// What a clean-up did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub removed: Vec<String>,
    /// Things that could not be removed, or that were still there afterwards.
    pub problems: Vec<String>,
}

impl Outcome {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    /// A short text for a dialog.
    pub fn describe(&self) -> String {
        let mut text = String::new();
        if self.removed.is_empty() && self.problems.is_empty() {
            text.push_str("Nothing had to be removed.");
        }
        for removed in &self.removed {
            text.push_str(&format!("Removed {removed}\n"));
        }
        for problem in &self.problems {
            text.push_str(&format!("Problem: {problem}\n"));
        }
        text.trim_end().to_string()
    }
}

/// Removes what the program has put on this machine outside its own configuration and log
/// folder, then checks that all of it is gone. The program file itself is not touched.
///
/// Notifications are switched off first, so that nothing registers again while this runs.
pub fn run() -> Outcome {
    notifier::disable();

    let before = notifier::leftovers();
    let mut problems = notifier::remove_leftovers();
    let after = notifier::leftovers();

    let removed = before
        .into_iter()
        .filter(|item| !after.contains(item))
        .collect();
    problems.extend(
        after
            .into_iter()
            .map(|item| format!("{item} is still there")),
    );
    Outcome { removed, problems }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_the_outcome() {
        assert_eq!(Outcome::default().describe(), "Nothing had to be removed.");
        assert!(Outcome::default().is_clean());

        let outcome = Outcome {
            removed: vec!["a".into()],
            problems: vec!["b is still there".into()],
        };
        assert_eq!(outcome.describe(), "Removed a\nProblem: b is still there");
        assert!(!outcome.is_clean());
    }
}

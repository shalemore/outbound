use std::collections::BTreeMap;

/// Files confirmed to match the remote during this run, as (size, modified time).
/// It lives in memory only: it saves every scan from re-checking files, and after a restart the
/// existing files are simply verified once more, never uploaded twice.
/// A change in either size or modified time makes a file count as new again.
#[derive(Default)]
pub struct Ledger {
    entries: BTreeMap<String, (u64, u64)>,
}

impl Ledger {
    pub fn is_synced(&self, key: &str, size: u64, modified: u64) -> bool {
        self.entries.get(key) == Some(&(size, modified))
    }

    pub fn insert(&mut self, key: String, size: u64, modified: u64) {
        self.entries.insert(key, (size, modified));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_changes() {
        let mut ledger = Ledger::default();
        ledger.insert("vps:/a".into(), 10, 100);
        assert!(ledger.is_synced("vps:/a", 10, 100));
        assert!(!ledger.is_synced("vps:/a", 11, 100));
        assert!(!ledger.is_synced("vps:/a", 10, 101));
        assert!(!ledger.is_synced("vps:/b", 10, 100));
    }
}

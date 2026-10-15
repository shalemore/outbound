use std::{fs, path::PathBuf};

/// The host aliases of `~/.ssh/config` that can be used as a destination: wildcard patterns and
/// negated patterns are skipped. A missing or unreadable file gives an empty list.
pub fn ssh_hosts() -> Vec<String> {
    let path =
        std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".ssh").join("config"));
    match path.map(fs::read_to_string) {
        Some(Ok(text)) => aliases(&text),
        Some(Err(e)) => {
            log::info!("no ssh hosts could be read from ~/.ssh/config: {e}");
            Vec::new()
        }
        None => Vec::new(),
    }
}

/// The names in the `Host` lines. Files that are pulled in with `Include` are not read.
fn aliases(config: &str) -> Vec<String> {
    let mut hosts: Vec<String> = config
        .lines()
        .filter_map(|line| {
            let (keyword, patterns) = line.trim().split_once([' ', '\t', '='])?;
            keyword.eq_ignore_ascii_case("host").then_some(patterns)
        })
        .flat_map(|patterns| {
            patterns
                .split_whitespace()
                .take_while(|pattern| !pattern.starts_with('#'))
        })
        .map(|pattern| pattern.trim_start_matches('='))
        .filter(|pattern| {
            !pattern.is_empty() && !pattern.starts_with('!') && !pattern.contains(['*', '?'])
        })
        .map(String::from)
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_plain_aliases_only() {
        let config = "Host vps-b vps-a\n  HostName 192.0.2.1\n\n\
                      Host *.internal !bad\n  User root\n\n\
                      Host vps-a\n  Port 2222\n\n\
                      host=lower # a comment\n\n\
                      Host *\n  ServerAliveInterval 30\n";
        assert_eq!(aliases(config), ["lower", "vps-a", "vps-b"]);
    }

    #[test]
    fn an_empty_config_has_no_hosts() {
        assert!(aliases("").is_empty());
    }
}

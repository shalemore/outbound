use std::{
    fs,
    path::{Path, PathBuf},
};

/// How many files may include one another. ssh itself stops at a similar depth.
const MAX_INCLUDE_DEPTH: usize = 4;

/// The host aliases of `~/.ssh/config` that can be used as a destination: wildcard patterns and
/// negated patterns are skipped. Files that are pulled in with `Include` are read too. A missing
/// or unreadable file gives an empty list.
pub fn ssh_hosts() -> Vec<String> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
    match fs::read_to_string(&path) {
        Ok(text) => {
            let mut hosts = aliases(&text, &folder, 0);
            hosts.sort_unstable();
            hosts.dedup();
            hosts
        }
        Err(e) => {
            log::info!("no ssh hosts could be read from ~/.ssh/config: {e}");
            Vec::new()
        }
    }
}

/// `~/.ssh/config`, where the system ssh reads its hosts from.
pub fn config_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".ssh").join("config"))
}

/// The names in the `Host` lines of the text and of the files it includes. A relative `Include`
/// path starts in `folder`, which is `~/.ssh`.
fn aliases(config: &str, folder: &Path, depth: usize) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in config.lines() {
        let Some((keyword, arguments)) = line.trim().split_once([' ', '\t', '=']) else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("include") && depth < MAX_INCLUDE_DEPTH {
            for pattern in arguments.split_whitespace().map(|p| p.trim_matches('"')) {
                for file in included_files(pattern, folder) {
                    if let Ok(text) = fs::read_to_string(&file) {
                        hosts.extend(aliases(&text, folder, depth + 1));
                    }
                }
            }
        } else if keyword.eq_ignore_ascii_case("host") {
            hosts.extend(
                arguments
                    .split_whitespace()
                    .take_while(|pattern| !pattern.starts_with('#'))
                    .map(|pattern| pattern.trim_start_matches('='))
                    .filter(|pattern| {
                        !pattern.is_empty()
                            && !pattern.starts_with('!')
                            && !pattern.contains(['*', '?'])
                    })
                    .map(String::from),
            );
        }
    }
    hosts
}

/// The files an `Include` path stands for. Only the last part of the path may have wildcards,
/// which is how it is almost always written (`config.d/*`).
fn included_files(pattern: &str, folder: &Path) -> Vec<PathBuf> {
    let pattern = pattern.replace('\\', "/");
    let pattern = match pattern.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("USERPROFILE") {
            Some(home) => PathBuf::from(home).join(rest),
            None => return Vec::new(),
        },
        None => folder.join(&pattern),
    };
    let (Some(parent), Some(name)) = (pattern.parent(), pattern.file_name()) else {
        return Vec::new();
    };
    let name = name.to_string_lossy();
    if !name.contains(['*', '?']) {
        return vec![pattern];
    }
    let mut files: Vec<PathBuf> = fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| wildcard_matches(&name, &entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
}

/// `*` stands for any text and `?` for one character. Windows ignores the case of file names.
fn wildcard_matches(pattern: &str, name: &str) -> bool {
    fn matches(pattern: &[char], name: &[char]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some(('*', rest)) => (0..=name.len()).any(|skip| matches(rest, &name[skip..])),
            Some((expected, rest)) => name.split_first().is_some_and(|(c, tail)| {
                (*expected == '?' || *expected == *c) && matches(rest, tail)
            }),
        }
    }
    let lower = |text: &str| text.to_lowercase().chars().collect::<Vec<_>>();
    matches(&lower(pattern), &lower(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts_of(config: &str) -> Vec<String> {
        let mut hosts = aliases(config, Path::new("."), 0);
        hosts.sort_unstable();
        hosts.dedup();
        hosts
    }

    #[test]
    fn lists_plain_aliases_only() {
        let config = "Host vps-b vps-a
  HostName 192.0.2.1

                      Host *.internal !bad
  User root

                      Host vps-a
  Port 2222

                      host=lower # a comment

                      Host *
  ServerAliveInterval 30
";
        assert_eq!(hosts_of(config), ["lower", "vps-a", "vps-b"]);
    }

    #[test]
    fn an_empty_config_has_no_hosts() {
        assert!(hosts_of("").is_empty());
    }

    #[test]
    fn included_files_are_read() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("config.d")).unwrap();
        fs::write(
            dir.path().join("config.d").join("a.conf"),
            "Host from-a
",
        )
        .unwrap();
        fs::write(
            dir.path().join("config.d").join("b.txt"),
            "Host not-included
",
        )
        .unwrap();
        fs::write(
            dir.path().join("extra"),
            "Host from-extra
Include extra
",
        )
        .unwrap();

        let config = "Include config.d/*.conf
Include \"extra\"
Host main
";
        let mut hosts = aliases(config, dir.path(), 0);
        hosts.sort_unstable();
        hosts.dedup();
        assert_eq!(hosts, ["from-a", "from-extra", "main"]);
    }

    #[test]
    fn wildcards_match_names() {
        assert!(wildcard_matches("*.conf", "A.CONF"));
        assert!(wildcard_matches("a?c", "abc"));
        assert!(!wildcard_matches("a?c", "ac"));
        assert!(wildcard_matches("*", ""));
        assert!(!wildcard_matches("*.conf", "a.txt"));
    }
}

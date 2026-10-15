use ssh2_config::{ParseRule, SshConfig};

/// The host aliases of `~/.ssh/config` that can be used as a destination: wildcard patterns and
/// negated patterns are skipped. A missing or unreadable file gives an empty list.
pub fn ssh_hosts() -> Vec<String> {
    let rules = ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS;
    match SshConfig::parse_default_file(rules) {
        Ok(config) => aliases(&config),
        Err(e) => {
            tracing::info!("no ssh hosts could be read from ~/.ssh/config: {e}");
            Vec::new()
        }
    }
}

fn aliases(config: &SshConfig) -> Vec<String> {
    let mut hosts: Vec<String> = config
        .get_hosts()
        .iter()
        .flat_map(|host| &host.pattern)
        .filter(|clause| !clause.negated && !clause.pattern.contains(['*', '?']))
        .map(|clause| clause.pattern.clone())
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> SshConfig {
        let rules = ParseRule::ALLOW_UNKNOWN_FIELDS | ParseRule::ALLOW_UNSUPPORTED_FIELDS;
        SshConfig::default()
            .parse(&mut text.as_bytes(), rules)
            .unwrap()
    }

    #[test]
    fn lists_plain_aliases_only() {
        let config = parse(
            "Host vps-b vps-a\n  HostName 192.0.2.1\n\n\
             Host *.internal !bad\n  User root\n\n\
             Host vps-a\n  Port 2222\n\n\
             Host *\n  ServerAliveInterval 30\n",
        );
        assert_eq!(aliases(&config), ["vps-a", "vps-b"]);
    }

    #[test]
    fn an_empty_config_has_no_hosts() {
        assert!(aliases(&parse("")).is_empty());
    }
}

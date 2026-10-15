use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

/// What a configuration without a server says, also on the very first run.
const CHOOSE_HOST: &str = "Choose the SSH host, then add a folder, both in this menu";

const TEMPLATE: &str = r#"# outbound configuration. Changes take effect on the next scan.
# A file goes to the recycle bin once the server holds exactly the same content.
# The tray menu can also set the ssh host and add folder mappings; it writes this file.
# In paths, use forward slashes (D:/Downloads) or single-quoted strings ('D:\Downloads').

# Optional settings. The values below are the defaults. They must stay above the first table.
# How often every folder is scanned. New files are also noticed as soon as they appear.
# scan_interval_secs = 30
# A file is uploaded only after it has not been modified for this many seconds.
# settle_secs = 60
# Temporary extensions used by downloaders. These files are never uploaded.
# ignored_extensions = ["part", "crdownload", "tmp", "!qb", "aria2", "downloading", "opdownload"]
# Show a system notification when files were uploaded and when a conflict or a problem appears.
# notifications = true
# A failed upload is tried again this many times, waiting 2 s, then 4 s, then 8 s and so on.
# retries = 3
# retry_delay_secs = 2
# The most the uploads may send per second, for example "500 KiB/s" or "10 MiB/s". No limit if unset.
# bandwidth_limit = "10 MiB/s"

[ssh]
# The ssh target: an alias from ~/.ssh/config, or user@host. The tray menu can choose one.
# Authentication is left to the system ssh, so keys, ssh-agent and ProxyJump work as usual.
destination = ""
# port = 22
# identity_file = "C:/Users/you/.ssh/id_ed25519"

# Local folder -> folder on the server. Both folders must already exist. The tray menu adds them,
# and there can be as many as you like; they all run in the background together.
# [[mapping]]
# local = "D:/Downloads/outbox"
# remote = "/data/videos"
# Optional per-folder settings. They override the global ones above for this folder only.
# enabled = true
# settle_secs = 60
# ignored_extensions = ["part", "crdownload"]
"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub ssh: Ssh,
    /// There can be none, for example after the last folder was removed from the menu.
    #[serde(default, rename = "mapping")]
    pub mappings: Vec<Mapping>,
    #[serde(default = "default_scan_interval_secs")]
    pub scan_interval_secs: u64,
    #[serde(default = "default_settle_secs")]
    pub settle_secs: u64,
    #[serde(default = "default_ignored_extensions")]
    pub ignored_extensions: Vec<String>,
    #[serde(default = "default_notifications")]
    pub notifications: bool,
    /// How many times a failed upload is tried again before it is reported.
    #[serde(default = "default_retries")]
    pub retries: u32,
    /// The wait before the first retry; every further retry waits twice as long.
    #[serde(default = "default_retry_delay_secs")]
    pub retry_delay_secs: u64,
    pub bandwidth_limit: Option<Rate>,
}

/// A speed in bytes per second, written as a text like `"10 MiB/s"` or as a plain number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RateInput")]
pub struct Rate(pub u64);

#[derive(Deserialize)]
#[serde(untagged)]
enum RateInput {
    Bytes(u64),
    Text(String),
}

impl TryFrom<RateInput> for Rate {
    type Error = String;

    fn try_from(input: RateInput) -> Result<Self, String> {
        let bytes = match input {
            RateInput::Bytes(bytes) => bytes,
            RateInput::Text(text) => parse_rate(&text)?,
        };
        if bytes == 0 {
            return Err("the bandwidth limit must be more than zero".to_string());
        }
        Ok(Self(bytes))
    }
}

/// `"10 MiB/s"`, `"500KiB/s"`, `"1.5 MB/s"` or `"2048"`. KiB, MiB and GiB count in 1024s, KB, MB
/// and GB in 1000s, and the `/s` is optional.
fn parse_rate(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let text = text
        .strip_suffix("/s")
        .or_else(|| text.strip_suffix("/S"))
        .unwrap_or(text);
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("not a speed: {text:?}"))?;
    let unit = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        other => return Err(format!("unknown unit {other:?}, use B, KiB, MiB or GiB")),
    };
    Ok((number * unit).round() as u64)
}

fn default_retries() -> u32 {
    3
}

fn default_retry_delay_secs() -> u64 {
    2
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ssh {
    pub destination: String,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub local: PathBuf,
    pub remote: String,
    /// A disabled mapping stays in the file but is ignored.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// The settings below override the global ones for this folder only.
    pub settle_secs: Option<u64>,
    pub ignored_extensions: Option<Vec<String>>,
}

impl Mapping {
    #[cfg(test)]
    pub fn new(local: impl Into<PathBuf>, remote: &str) -> Self {
        Self {
            local: local.into(),
            remote: remote.to_string(),
            enabled: true,
            settle_secs: None,
            ignored_extensions: None,
        }
    }

    pub fn settle_secs(&self, config: &Config) -> u64 {
        self.settle_secs.unwrap_or(config.settle_secs)
    }

    pub fn ignored_extensions<'a>(&'a self, config: &'a Config) -> &'a [String] {
        self.ignored_extensions
            .as_deref()
            .unwrap_or(&config.ignored_extensions)
    }
}

fn default_enabled() -> bool {
    true
}

fn default_scan_interval_secs() -> u64 {
    30
}

fn default_notifications() -> bool {
    true
}

fn default_settle_secs() -> u64 {
    60
}

fn default_ignored_extensions() -> Vec<String> {
    [
        "part",
        "crdownload",
        "tmp",
        "!qb",
        "aria2",
        "downloading",
        "opdownload",
    ]
    .map(String::from)
    .into()
}

impl Config {
    /// Writes the template when the file is missing and returns an error so the user edits it.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir)?;
                }
                fs::write(path, TEMPLATE)?;
                bail!("{CHOOSE_HOST}");
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("invalid configuration ({})", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml_edit::de::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let destination = self.ssh.destination.trim();
        if destination.is_empty() {
            bail!("{CHOOSE_HOST}");
        }
        // A leading dash would be parsed by ssh as a command line option.
        if destination.starts_with('-') {
            bail!("ssh.destination must not start with a dash");
        }
        for mapping in &self.mappings {
            if !mapping.remote.starts_with('/') {
                bail!("remote must be an absolute path: {}", mapping.remote);
            }
        }
        if self.scan_interval_secs == 0 {
            bail!("scan_interval_secs must be greater than zero");
        }
        Ok(())
    }
}

/// `%APPDATA%\outbound\config.toml`
pub fn config_path() -> Result<PathBuf> {
    Ok(known_folder("APPDATA")?
        .join("outbound")
        .join("config.toml"))
}

/// `%LOCALAPPDATA%\outbound`, for the log, the lock and the notification icon.
pub fn data_dir() -> Result<PathBuf> {
    Ok(known_folder("LOCALAPPDATA")?.join("outbound"))
}

fn known_folder(variable: &str) -> Result<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .with_context(|| format!("%{variable}% is not set"))
}

/// Changes the configuration file in place and keeps its comments and layout. The result is
/// validated before it is written, and written through a temporary file so that the worker
/// thread never reads a half-written file.
pub fn edit(path: &Path, change: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => TEMPLATE.to_string(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let mut document: DocumentMut = text
        .parse()
        .context("the configuration file is not valid TOML")?;
    change(&mut document)?;
    let edited = document.to_string();
    Config::parse(&edited).context("the edited configuration would be invalid")?;

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("toml.tmp");
    fs::write(&temp, edited)?;
    fs::rename(&temp, path)?;
    Ok(())
}

pub fn set_destination(document: &mut DocumentMut, destination: &str) -> Result<()> {
    let ssh = document
        .entry("ssh")
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .context("`ssh` in the configuration is not a table")?;
    ssh["destination"] = value(destination.trim());
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    New,
    /// The local folder is already mapped, to this remote folder.
    AlreadyMapped(String),
}

pub fn add_mapping(document: &mut DocumentMut, local: &str, remote: &str) -> Result<Added> {
    let mappings = document
        .entry("mapping")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .context("`mapping` in the configuration is not an array of tables")?;
    let existing = mappings
        .iter()
        .find(|mapping| mapping.get("local").and_then(Item::as_str) == Some(local));
    if let Some(existing) = existing {
        let remote = existing.get("remote").and_then(Item::as_str);
        return Ok(Added::AlreadyMapped(remote.unwrap_or_default().to_string()));
    }

    let mut mapping = Table::new();
    mapping["local"] = value(local);
    mapping["remote"] = value(remote);
    mappings.push(mapping);
    Ok(Added::New)
}

/// The configured ssh destination, read leniently so that it also works while the rest of the
/// file is incomplete.
pub fn current_destination(path: &Path) -> Option<String> {
    let document: DocumentMut = fs::read_to_string(path).ok()?.parse().ok()?;
    let destination = document.get("ssh")?.get("destination")?.as_str()?;
    Some(destination.to_string())
}

/// One configured folder as the tray menu shows it.
#[derive(Debug, PartialEq, Eq)]
pub struct MappingInfo {
    pub local: String,
    pub remote: String,
    pub enabled: bool,
}

/// The configured folders, read leniently so that it also works while the rest of the file is
/// incomplete.
pub fn mappings_overview(path: &Path) -> Vec<MappingInfo> {
    let Some(document) = fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<DocumentMut>().ok())
    else {
        return Vec::new();
    };
    let Some(mappings) = document.get("mapping").and_then(Item::as_array_of_tables) else {
        return Vec::new();
    };
    mappings
        .iter()
        .filter_map(|mapping| {
            Some(MappingInfo {
                local: mapping.get("local")?.as_str()?.to_string(),
                remote: mapping.get("remote")?.as_str()?.to_string(),
                enabled: mapping
                    .get("enabled")
                    .and_then(Item::as_bool)
                    .unwrap_or(true),
            })
        })
        .collect()
}

/// Turns the mapping of the local folder on or off.
pub fn set_mapping_enabled(document: &mut DocumentMut, local: &str, enabled: bool) -> Result<()> {
    let mappings = document
        .get_mut("mapping")
        .and_then(Item::as_array_of_tables_mut)
        .context("the configuration has no mappings")?;
    let mapping = mappings
        .iter_mut()
        .find(|mapping| mapping.get("local").and_then(Item::as_str) == Some(local))
        .with_context(|| format!("{local} is not in the configuration"))?;
    mapping["enabled"] = value(enabled);
    Ok(())
}

/// Takes the mapping of the local folder out of the configuration. The last mapping takes the
/// whole `mapping` table with it, so that no empty array is left behind.
pub fn remove_mapping(document: &mut DocumentMut, local: &str) -> Result<()> {
    let mappings = document
        .get_mut("mapping")
        .and_then(Item::as_array_of_tables_mut)
        .context("the configuration has no mappings")?;
    let position = mappings
        .iter()
        .position(|mapping| mapping.get("local").and_then(Item::as_str) == Some(local))
        .with_context(|| format!("{local} is not in the configuration"))?;
    mappings.remove(position);
    if mappings.is_empty() {
        document.remove("mapping");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [ssh]
        destination = "vps"
        [[mapping]]
        local = "D:/a"
        remote = "/data/a"
    "#;

    #[test]
    fn template_asks_for_the_host_and_is_valid_once_it_is_set() {
        let error = Config::parse(TEMPLATE).unwrap_err().to_string();
        assert!(error.contains("Choose the SSH host"), "{error}");
        Config::parse(&configured()).unwrap();
    }

    /// The template with a host and the example folder, as after setting it up.
    fn configured() -> String {
        TEMPLATE
            .replace(r#"destination = """#, r#"destination = "vps""#)
            .replace(
                "# [[mapping]]\n# local = \"D:/Downloads/outbox\"\n# remote = \"/data/videos\"",
                "[[mapping]]\nlocal = \"D:/Downloads/outbox\"\nremote = \"/data/videos\"",
            )
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let config = Config::parse(MINIMAL).unwrap();
        assert_eq!(config.scan_interval_secs, 30);
        assert_eq!(config.settle_secs, 60);
        assert!(config.ignored_extensions.contains(&"part".to_string()));
    }

    #[test]
    fn rejects_relative_remote() {
        let text = MINIMAL.replace("/data/a", "data/a");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn rejects_option_like_destination() {
        let text = MINIMAL.replace("\"vps\"", "\"-oProxyCommand=x\"");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Config::parse(&format!("unknown = 1\n{MINIMAL}")).is_err());
    }

    #[test]
    fn a_configuration_may_have_no_folders() {
        let config = Config::parse("[ssh]\ndestination = \"vps\"").unwrap();
        assert!(config.mappings.is_empty());
    }

    #[test]
    fn every_optional_setting_in_the_template_can_be_enabled() {
        const OPTIONAL: [&str; 10] = [
            "scan_interval_secs",
            "settle_secs",
            "ignored_extensions",
            "notifications",
            "retries",
            "retry_delay_secs",
            "bandwidth_limit",
            "enabled",
            "port",
            "identity_file",
        ];
        let configured = configured();
        let enabled: Vec<&str> = configured
            .lines()
            .map(|line| match line.strip_prefix("# ") {
                Some(rest) if OPTIONAL.iter().any(|name| rest.starts_with(name)) => rest,
                _ => line,
            })
            .collect();
        let config = Config::parse(&enabled.join("\n")).unwrap();
        assert_eq!(config.ssh.port, Some(22));
        assert_eq!(config.settle_secs, 60);
        assert!(config.mappings[0].enabled);
    }

    fn temp_config(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn edit_keeps_comments_and_sets_the_destination() {
        let (_dir, path) = temp_config(TEMPLATE);
        edit(&path, |doc| set_destination(doc, "my-vps")).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("destination = \"my-vps\""));
        assert!(
            text.contains("# scan_interval_secs = 30"),
            "comments are kept"
        );
        assert_eq!(Config::parse(&text).unwrap().ssh.destination, "my-vps");
        assert_eq!(current_destination(&path).as_deref(), Some("my-vps"));
    }

    #[test]
    fn edit_creates_the_file_from_the_template() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        edit(&path, |doc| set_destination(doc, "my-vps")).unwrap();
        assert_eq!(current_destination(&path).as_deref(), Some("my-vps"));
    }

    #[test]
    fn adds_a_mapping_once() {
        let (_dir, path) = temp_config(&configured());
        let mut outcomes = Vec::new();
        for remote in ["/data/new", "/data/other"] {
            edit(&path, |doc| {
                outcomes.push(add_mapping(doc, "D:/Videos", remote)?);
                Ok(())
            })
            .unwrap();
        }

        assert_eq!(
            outcomes,
            [Added::New, Added::AlreadyMapped("/data/new".into())]
        );
        let config = Config::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.mappings.len(), 2);
        assert_eq!(config.mappings[1].remote, "/data/new");
    }

    #[test]
    fn an_invalid_edit_is_not_written() {
        let (_dir, path) = temp_config(&configured());
        let result = edit(&path, |doc| {
            add_mapping(doc, "D:/Videos", "relative/path").map(|_| ())
        });

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), configured());
    }

    #[test]
    fn a_mapping_overrides_the_global_settings() {
        let text = r#"
            settle_secs = 10

            [ssh]
            destination = "vps"

            [[mapping]]
            local = "D:/a"
            remote = "/a"
            settle_secs = 99
            ignored_extensions = ["x"]

            [[mapping]]
            local = "D:/b"
            remote = "/b"
            enabled = false
        "#;
        let config = Config::parse(text).unwrap();
        let (first, second) = (&config.mappings[0], &config.mappings[1]);

        assert_eq!(first.settle_secs(&config), 99);
        assert_eq!(first.ignored_extensions(&config), ["x"]);
        assert!(first.enabled);

        assert_eq!(second.settle_secs(&config), 10);
        assert_eq!(
            second.ignored_extensions(&config),
            config.ignored_extensions
        );
        assert!(!second.enabled);
    }

    #[test]
    fn lists_and_toggles_the_mappings() {
        let (_dir, path) = temp_config(&configured());
        edit(&path, |doc| {
            add_mapping(doc, "D:/Videos", "/data/new").map(|_| ())
        })
        .unwrap();
        assert_eq!(
            mappings_overview(&path),
            [
                MappingInfo {
                    local: "D:/Downloads/outbox".into(),
                    remote: "/data/videos".into(),
                    enabled: true
                },
                MappingInfo {
                    local: "D:/Videos".into(),
                    remote: "/data/new".into(),
                    enabled: true
                },
            ]
        );

        edit(&path, |doc| set_mapping_enabled(doc, "D:/Videos", false)).unwrap();
        let overview = mappings_overview(&path);
        assert!(overview[0].enabled && !overview[1].enabled);
        let config = Config::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!config.mappings[1].enabled);

        assert!(edit(&path, |doc| set_mapping_enabled(doc, "D:/Nope", true)).is_err());
    }

    #[test]
    fn removes_a_mapping_and_keeps_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# my notes
[ssh]
destination = \"vps\"

[[mapping]]
local = \"D:/a\"
remote = \"/data/a\"

[[mapping]]
local = \"D:/b\"
remote = \"/data/b\"
",
        )
        .unwrap();

        edit(&path, |doc| remove_mapping(doc, "D:/a")).unwrap();
        let overview = mappings_overview(&path);
        assert_eq!(overview.len(), 1);
        assert_eq!(overview[0].local, "D:/b");
        assert!(fs::read_to_string(&path).unwrap().starts_with("# my notes"));

        assert!(edit(&path, |doc| remove_mapping(doc, "D:/a")).is_err());
        edit(&path, |doc| remove_mapping(doc, "D:/b")).unwrap();
        assert!(mappings_overview(&path).is_empty());
    }

    #[test]
    fn parses_speeds() {
        assert_eq!(parse_rate("10 MiB/s").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_rate("500KiB/s").unwrap(), 500 * 1024);
        assert_eq!(parse_rate("1.5 MB/s").unwrap(), 1_500_000);
        assert_eq!(parse_rate("2048").unwrap(), 2048);
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("10 TB/s").is_err());
    }

    #[test]
    fn reads_the_retry_and_speed_settings() {
        let defaults = Config::parse(MINIMAL).unwrap();
        assert_eq!((defaults.retries, defaults.retry_delay_secs), (3, 2));
        assert_eq!(defaults.bandwidth_limit, None);

        let text =
            format!("retries = 0\nretry_delay_secs = 5\nbandwidth_limit = \"1 MiB/s\"\n{MINIMAL}");
        let config = Config::parse(&text).unwrap();
        assert_eq!((config.retries, config.retry_delay_secs), (0, 5));
        assert_eq!(config.bandwidth_limit, Some(Rate(1024 * 1024)));

        let number = format!("bandwidth_limit = 4096\n{MINIMAL}");
        assert_eq!(
            Config::parse(&number).unwrap().bandwidth_limit,
            Some(Rate(4096))
        );
        let zero = format!("bandwidth_limit = 0\n{MINIMAL}");
        assert!(Config::parse(&zero).is_err());
        let nonsense = format!("bandwidth_limit = \"quick\"\n{MINIMAL}");
        assert!(Config::parse(&nonsense).is_err());
    }
}

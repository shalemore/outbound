use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc::Sender},
    thread,
};

use anyhow::{Context, Result, bail};
use tao::event_loop::EventLoopProxy;

use crate::{
    UserEvent,
    config::{self, Added, Config},
    dialog,
    ssh::SshRemote,
    sync::Remote,
    worker::Command,
};

/// A message for the user, shown in a dialog by the thread that runs the event loop.
pub enum Notice {
    Info(String),
    Error(String),
}

impl Notice {
    pub fn show(&self) {
        match self {
            Self::Info(message) => dialog::info(message),
            Self::Error(message) => dialog::error(message),
        }
    }

    fn from_result(result: Result<String>) -> Self {
        match result {
            Ok(message) => Self::Info(message),
            Err(e) => Self::Error(format!("{e:#}")),
        }
    }
}

/// Makes `host` the ssh destination in the configuration file.
pub fn use_host(host: &str) -> Result<()> {
    config::edit(&config::config_path()?, |document| {
        config::set_destination(document, host)
    })
}

/// Turns the mapping of a local folder on or off in the configuration file.
pub fn set_folder_enabled(local: &str, enabled: bool) -> Result<()> {
    config::edit(&config::config_path()?, |document| {
        config::set_mapping_enabled(document, local, enabled)
    })
}

/// Takes the mapping of a local folder out of the configuration file.
pub fn remove_folder(local: &str) -> Result<()> {
    config::edit(&config::config_path()?, |document| {
        config::remove_mapping(document, local)
    })
}

/// First step of adding a folder mapping: pick the local folder, then ask the VPS for its home
/// directory in the background so that the next dialog can suggest a remote folder.
pub fn start_add_mapping(proxy: &EventLoopProxy<UserEvent>) {
    let Some(local) = dialog::pick_folder("Choose the local folder to send to the VPS") else {
        return;
    };
    let proxy = proxy.clone();
    thread::spawn(move || {
        let event = match remote_home() {
            Ok(home) => UserEvent::AskRemote { local, home },
            Err(e) => UserEvent::Notice(Notice::Error(format!("{e:#}"))),
        };
        let _ = proxy.send_event(event);
    });
}

fn remote_home() -> Result<String> {
    let config = Config::load(&config::config_path()?)?;
    SshRemote::new(&config.ssh)
        .home_dir()
        .context("cannot reach the VPS, check the ssh host first")
}

/// Second step: ask for the remote folder, then check it and write the mapping in the background.
pub fn ask_remote(
    local: PathBuf,
    home: &str,
    proxy: &EventLoopProxy<UserEvent>,
    commands: &Sender<Command>,
) {
    let ssh = match config::config_path().and_then(|path| Config::load(&path)) {
        Ok(config) => config.ssh,
        Err(e) => {
            Notice::Error(format!("{e:#}")).show();
            return;
        }
    };
    let lookup = Arc::new(move |path: &str| SshRemote::new(&ssh).browse(path));
    let Some(answer) = dialog::ask_remote_folder(&local, &suggested_remote(home, &local), lookup)
    else {
        return;
    };
    let (proxy, commands) = (proxy.clone(), commands.clone());
    thread::spawn(move || {
        let result = add_mapping(&local, &answer);
        if result.is_ok() {
            let _ = commands.send(Command::SyncNow);
        }
        let _ = proxy.send_event(UserEvent::Notice(Notice::from_result(result)));
    });
}

fn add_mapping(local: &Path, remote: &str) -> Result<String> {
    let remote = normalize_remote(remote)?;
    let local = local
        .to_str()
        .context("the local folder path is not valid UTF-8")?
        .replace('\\', "/");

    let path = config::config_path()?;
    let config = Config::load(&path)?;
    let exists = SshRemote::new(&config.ssh)
        .dir_exists(&remote)
        .context("cannot reach the VPS")?;
    if !exists {
        bail!("{remote} does not exist on the VPS, create it first");
    }

    let mut added = None;
    config::edit(&path, |document| {
        added = Some(config::add_mapping(document, &local, &remote)?);
        Ok(())
    })?;
    Ok(match added {
        Some(Added::AlreadyMapped(existing)) => format!("{local} is already sent to {existing}"),
        _ => format!("Added: {local} -> {remote}"),
    })
}

/// The remote folder must be an absolute path. A trailing slash is dropped, because the
/// configuration and the paths built from it use none.
fn normalize_remote(input: &str) -> Result<String> {
    let input = input.trim();
    if !input.starts_with('/') {
        bail!("the VPS folder must be an absolute path such as /data/videos, not {input:?}");
    }
    let trimmed = input.trim_end_matches('/');
    Ok(if trimmed.is_empty() { "/" } else { trimmed }.to_string())
}

/// The home directory of the VPS plus the name of the local folder.
fn suggested_remote(home: &str, local: &Path) -> String {
    let home = home.trim_end_matches('/');
    match local.file_name().and_then(|name| name.to_str()) {
        Some(name) => format!("{home}/{name}"),
        None => home.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_the_remote_folder() {
        assert_eq!(normalize_remote(" /data/videos/ ").unwrap(), "/data/videos");
        assert_eq!(normalize_remote("/").unwrap(), "/");
        assert!(normalize_remote("data/videos").is_err());
        assert!(normalize_remote("~/videos").is_err());
        assert!(normalize_remote("   ").is_err());
    }

    #[test]
    fn suggests_home_plus_the_folder_name() {
        assert_eq!(
            suggested_remote("/home/me", Path::new("D:/Downloads/outbox")),
            "/home/me/outbox"
        );
        assert_eq!(suggested_remote("/root/", Path::new("D:/x")), "/root/x");
        assert_eq!(suggested_remote("/root", Path::new("/")), "/root");
    }
}

use std::{
    path::Path,
    sync::{Arc, mpsc::Sender},
};

use anyhow::{Context, Result, bail};

use crate::{
    config::{self, Added, Config},
    dialog, remote_dialog,
    ssh::SshRemote,
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

/// Adds a folder mapping: the local folder comes from the system folder picker, and the folder
/// on the server from a dialog that checks it while it is typed, so it is known to exist.
pub fn add_folder(commands: &Sender<Command>) {
    let Some(local) = dialog::pick_folder("Choose the local folder to send to the server") else {
        return;
    };
    let ssh = match config::config_path().and_then(|path| Config::load(&path)) {
        Ok(config) => config.ssh,
        Err(e) => {
            Notice::Error(format!("{e:#}")).show();
            return;
        }
    };
    let Some(remote) = remote_dialog::ask(&local, Arc::new(SshRemote::new(&ssh))) else {
        return;
    };
    match add_mapping(&local, &remote) {
        Ok(Added::New) => {
            let _ = commands.send(Command::SyncNow);
        }
        Ok(Added::AlreadyMapped(existing)) => Notice::Info(format!(
            "{} is already sent to {existing}.",
            local.display()
        ))
        .show(),
        Err(e) => Notice::Error(format!("{e:#}")).show(),
    }
}

fn add_mapping(local: &Path, remote: &str) -> Result<Added> {
    let remote = normalize_remote(remote)?;
    let local = local
        .to_str()
        .context("the local folder path is not valid UTF-8")?
        .replace('\\', "/");
    let mut added = None;
    config::edit(&config::config_path()?, |document| {
        added = Some(config::add_mapping(document, &local, &remote)?);
        Ok(())
    })?;
    added.context("the configuration was not changed")
}

/// The remote folder must be an absolute path. A trailing slash is dropped, because the
/// configuration and the paths built from it use none.
fn normalize_remote(input: &str) -> Result<String> {
    let input = input.trim();
    if !input.starts_with('/') {
        bail!(
            "the folder on the server must be an absolute path such as /data/videos, not {input:?}"
        );
    }
    let trimmed = input.trim_end_matches('/');
    Ok(if trimmed.is_empty() { "/" } else { trimmed }.to_string())
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
}

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;

use crate::ssh::Browse;
use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

const TITLE: &str = "Outbound";

/// Shows the system folder picker. These functions block, so they are called from the thread
/// that runs the event loop; macOS only allows dialogs there.
pub fn pick_folder(description: &str) -> Option<PathBuf> {
    FileDialog::new().set_title(description).pick_folder()
}

/// What the dialog for a remote folder asks the VPS about a typed path. It runs on a thread of
/// its own.
pub type Lookup = Arc<dyn Fn(&str) -> Result<Browse> + Send + Sync>;

/// Asks for the folder on the VPS that receives `local`, starting from `initial`. None means the
/// user cancelled.
#[cfg(windows)]
pub fn ask_remote_folder(local: &Path, initial: &str, lookup: Lookup) -> Option<String> {
    crate::remote_dialog::ask(local, initial, lookup).filter(|text| !text.is_empty())
}

/// Other systems get a plain input box, without the check of the folder.
#[cfg(not(windows))]
pub fn ask_remote_folder(local: &Path, initial: &str, _lookup: Lookup) -> Option<String> {
    let message = format!(
        "Folder on the VPS that receives
{}",
        local.display()
    );
    tinyfiledialogs::input_box(TITLE, &message, initial).filter(|text| !text.trim().is_empty())
}

/// A yes or no question. Closing the dialog counts as no.
pub fn confirm(message: &str) -> bool {
    MessageDialog::new()
        .set_level(MessageLevel::Warning)
        .set_title(TITLE)
        .set_description(message)
        .set_buttons(MessageButtons::YesNo)
        .show()
        == MessageDialogResult::Yes
}

pub fn info(message: &str) {
    show(MessageLevel::Info, message);
}

pub fn error(message: &str) {
    show(MessageLevel::Error, message);
}

fn show(level: MessageLevel, message: &str) {
    MessageDialog::new()
        .set_level(level)
        .set_title(TITLE)
        .set_description(message)
        .set_buttons(MessageButtons::Ok)
        .show();
}

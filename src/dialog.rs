use std::path::PathBuf;

use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

const TITLE: &str = "Outbound";

/// Shows the system folder picker. These functions block, so they are called from the thread
/// that runs the event loop; macOS only allows dialogs there.
pub fn pick_folder(description: &str) -> Option<PathBuf> {
    FileDialog::new().set_title(description).pick_folder()
}

/// Asks for a line of text, starting from `default`. None means the user cancelled.
pub fn ask_text(message: &str, default: &str) -> Option<String> {
    tinyfiledialogs::input_box(TITLE, message, default).filter(|text| !text.trim().is_empty())
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

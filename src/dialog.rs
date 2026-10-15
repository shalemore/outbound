use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree,
        },
        UI::{
            Shell::{
                FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
                SIGDN_FILESYSPATH,
            },
            WindowsAndMessaging::{
                IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO,
                MESSAGEBOX_STYLE, MessageBoxW,
            },
        },
    },
    core::{HSTRING, w},
};

use crate::ssh::Browse;

/// What the dialog for a remote folder asks the VPS about a typed path. It runs on a thread of
/// its own.
pub type Lookup = Arc<dyn Fn(&str) -> Result<Browse> + Send + Sync>;

/// Shows the system folder picker. None means the user cancelled. These functions block, so they
/// are called from the thread that runs the event loop.
pub fn pick_folder(title: &str) -> Option<PathBuf> {
    // SAFETY: plain COM calls on the thread that makes them; the string that the picker returns is
    // freed once it has been copied.
    unsafe {
        // Already initialized by the event loop is fine; the answer does not matter.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .ok()?;
        dialog.SetTitle(&HSTRING::from(title)).ok()?;
        // Cancelling is an error result.
        dialog.Show(None).ok()?;
        let name = dialog
            .GetResult()
            .ok()?
            .GetDisplayName(SIGDN_FILESYSPATH)
            .ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0.cast()));
        path
    }
}

/// Asks for the folder on the VPS that receives `local`, starting from `initial`. None means the
/// user cancelled.
pub fn ask_remote_folder(local: &std::path::Path, initial: &str, lookup: Lookup) -> Option<String> {
    crate::remote_dialog::ask(local, initial, lookup).filter(|text| !text.is_empty())
}

/// A yes or no question. Closing the dialog counts as no.
pub fn confirm(message: &str) -> bool {
    show(message, MB_YESNO | MB_ICONWARNING) == IDYES.0
}

pub fn info(message: &str) {
    show(message, MB_OK | MB_ICONINFORMATION);
}

pub fn error(message: &str) {
    show(message, MB_OK | MB_ICONERROR);
}

fn show(message: &str, style: MESSAGEBOX_STYLE) -> i32 {
    // SAFETY: both strings live until the call returns.
    unsafe { MessageBoxW(None, &HSTRING::from(message), w!("Outbound"), style).0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opens the pickers and boxes, to look at them. Run it with
    /// `cargo test --release show_the_pickers -- --ignored`.
    #[test]
    #[ignore]
    fn show_the_pickers() {
        eprintln!(
            "picked: {:?}",
            pick_folder("Choose the local folder to send to the VPS")
        );
        eprintln!(
            "confirmed: {}",
            confirm(
                "Stop sending D:/a to /data/a and remove it from the configuration?\n\nNothing is deleted, neither here nor on the VPS."
            )
        );
    }
}

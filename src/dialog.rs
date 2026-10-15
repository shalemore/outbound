use std::path::PathBuf;

use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree,
        },
        UI::{
            Controls::{
                TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TDF_ALLOW_DIALOG_CANCELLATION,
                TaskDialogIndirect,
            },
            Shell::{
                FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
                SIGDN_FILESYSPATH,
            },
            WindowsAndMessaging::{
                MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_STYLE, MessageBoxW,
            },
        },
    },
    core::{HSTRING, PCWSTR, w},
};

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

/// A question with a heading and a smaller explanation below it. The button that does it is
/// called `action`; closing the dialog counts as cancelling.
pub fn confirm(heading: &str, details: &str, action: &str) -> bool {
    const ACTION: i32 = 100;
    const CANCEL: i32 = 101;
    let (title, heading, details) = (
        HSTRING::from("Outbound"),
        HSTRING::from(heading),
        HSTRING::from(details),
    );
    let (action_text, cancel_text) = (HSTRING::from(action), HSTRING::from("Cancel"));
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: ACTION,
            pszButtonText: PCWSTR(action_text.as_ptr()),
        },
        TASKDIALOG_BUTTON {
            nButtonID: CANCEL,
            pszButtonText: PCWSTR(cancel_text.as_ptr()),
        },
    ];
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszMainInstruction: PCWSTR(heading.as_ptr()),
        pszContent: PCWSTR(details.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: CANCEL,
        ..Default::default()
    };
    let mut pressed = 0;
    // SAFETY: the strings and the buttons live until the call returns.
    let shown = unsafe { TaskDialogIndirect(&config, Some(&mut pressed), None, None) };
    shown.is_ok() && pressed == ACTION
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

    /// Opens the folder picker, to look at it. Run it with
    /// `cargo test --release show_the_picker -- --ignored`.
    #[test]
    #[ignore]
    fn show_the_picker() {
        eprintln!(
            "picked: {:?}",
            pick_folder("Choose the local folder to send to the VPS")
        );
    }

    /// Opens the question, to look at it. Run it with
    /// `cargo test --release show_the_question -- --ignored`.
    #[test]
    #[ignore]
    fn show_the_question() {
        let heading = r"Remove D:\Downloads\outbox?";
        let details = "It stops being sent to /data/videos. No file is deleted.";
        eprintln!("confirmed: {}", confirm(heading, details, "Remove"));
    }
}

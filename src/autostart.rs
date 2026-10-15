//! Starting with Windows: one value in the Run key of the current user.

use std::io;

use winreg::{
    RegKey,
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE},
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "outbound";

/// The command that starts this executable. The path is quoted, because Windows splits the value
/// at spaces.
fn command() -> io::Result<String> {
    Ok(format!("\"{}\"", std::env::current_exe()?.display()))
}

pub fn is_enabled() -> bool {
    current(VALUE).is_some()
}

/// Turns starting with Windows on or off.
pub fn set(enabled: bool) -> io::Result<()> {
    if enabled {
        write(VALUE, &command()?)
    } else {
        remove(VALUE)
    }
}

/// Keeps an enabled entry pointing at this executable, which is the one that is running, so that
/// moving the program does not break it.
pub fn refresh() {
    let Ok(wanted) = command() else { return };
    if current(VALUE).is_some_and(|value| value != wanted)
        && let Err(e) = write(VALUE, &wanted)
    {
        log::warn!("cannot update the startup entry: {e}");
    }
}

/// What the entry is, as a readable description, when there is one.
pub fn leftovers() -> Vec<String> {
    if is_enabled() {
        vec![format!("the startup entry (HKCU\\{RUN_KEY}\\{VALUE})")]
    } else {
        Vec::new()
    }
}

/// Removes the entry. A missing entry is not an error.
pub fn remove_leftovers() -> Option<String> {
    remove(VALUE)
        .err()
        .map(|e| format!("cannot remove the startup entry: {e}"))
}

fn current(name: &str) -> Option<String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .ok()?
        .get_value(name)
        .ok()
}

fn write(name: &str, command: &str) -> io::Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    key.set_value(name, &command)
}

fn remove(name: &str) -> io::Result<()> {
    let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_WRITE)
    else {
        return Ok(());
    };
    match key.delete_value(name) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_reads_and_removes_the_entry() {
        let name = format!("outbound-test-{}", std::process::id());
        assert_eq!(current(&name), None);

        write(&name, r#""C:\Program Files\outbound.exe""#).unwrap();
        assert_eq!(
            current(&name).as_deref(),
            Some(r#""C:\Program Files\outbound.exe""#)
        );

        remove(&name).unwrap();
        assert_eq!(current(&name), None);
        // Removing what is not there is not an error.
        remove(&name).unwrap();
    }

    #[test]
    fn the_command_quotes_the_path() {
        let command = command().unwrap();
        assert!(
            command.starts_with('"') && command.ends_with('"'),
            "{command}"
        );
    }
}

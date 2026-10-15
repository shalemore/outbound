use std::sync::atomic::{AtomicBool, Ordering};

use tauri_winrt_notification::Toast;

const NAME: &str = "Outbound";
/// Toasts on Windows are attributed to an application id, and a program that is not installed
/// has to register one. Without it they would be shown as coming from Windows PowerShell.
const APP_ID: &str = "shalemore.outbound";
/// Toast bodies are cut off by the system anyway, so there is no point in sending long ones.
const MAX_BODY_CHARS: usize = 200;

/// Set once the program has cleaned up after itself, so that nothing registers again.
static DISABLED: AtomicBool = AtomicBool::new(false);

/// Shows a system notification. A failure is only logged: a missing notification service must
/// never get in the way of the sync.
pub fn send(title: &str, body: &str) {
    if DISABLED.load(Ordering::Relaxed) {
        return;
    }
    registration::ensure(APP_ID, NAME);

    let body: String = body.chars().take(MAX_BODY_CHARS).collect();
    if let Err(e) = Toast::new(APP_ID).title(title).text1(&body).show() {
        tracing::warn!("cannot show a notification: {e}");
    }
}

/// From now on `send` does nothing.
pub fn disable() {
    DISABLED.store(true, Ordering::Relaxed);
}

/// What this program has put on the system for its notifications, as readable descriptions.
pub fn leftovers() -> Vec<String> {
    registration::leftovers(APP_ID, registration::icon_path().as_deref())
}

/// Removes what `leftovers` lists and returns the errors, if any.
pub fn remove_leftovers() -> Vec<String> {
    registration::remove(APP_ID, registration::icon_path().as_deref())
        .err()
        .map(|e| format!("cannot remove the notification registration: {e}"))
        .into_iter()
        .collect()
}

mod registration {
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::Once,
    };

    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    use crate::config;

    const ID_KEY: &str = r"Software\Classes\AppUserModelId";
    /// Windows creates this one itself when the first toast is shown. It holds the choices made in
    /// the notification settings.
    const SETTINGS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Notifications\Settings";
    const ICON: &[u8] = include_bytes!("../assets/icon-256.png");

    /// Toasts and the notification settings show an icon only if it is a file on disk, so the
    /// one that is built into the executable is written next to the configuration.
    pub fn icon_path() -> Option<PathBuf> {
        Some(config::data_dir().ok()?.join("icon.png"))
    }

    /// Registers the application id once per run, and only when a notification is really sent,
    /// so a run that never notifies leaves nothing behind.
    pub fn ensure(app_id: &str, display_name: &str) {
        static REGISTERED: Once = Once::new();
        REGISTERED.call_once(|| {
            let icon = icon_path().and_then(|path| write_icon(&path).ok().map(|()| path));
            if let Err(e) = register(app_id, display_name, icon.as_deref()) {
                tracing::warn!("cannot register the notification name: {e}");
            }
        });
    }

    fn write_icon(path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, ICON)
    }

    pub fn register(app_id: &str, display_name: &str, icon: Option<&Path>) -> io::Result<()> {
        let user = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = user.create_subkey(format!(r"{ID_KEY}\{app_id}"))?;
        key.set_value("DisplayName", &display_name)?;
        if let Some(icon) = icon {
            key.set_value("IconUri", &icon.to_string_lossy().as_ref())?;
        }
        // Lets the user mute the program from the notification settings.
        key.set_value("ShowInSettings", &1u32)?;
        Ok(())
    }

    pub fn leftovers(app_id: &str, icon: Option<&Path>) -> Vec<String> {
        let user = RegKey::predef(HKEY_CURRENT_USER);
        let mut found = Vec::new();
        for (base, what) in [
            (ID_KEY, "the notification name"),
            (SETTINGS_KEY, "the notification settings entry"),
        ] {
            let path = format!(r"{base}\{app_id}");
            if user.open_subkey(&path).is_ok() {
                found.push(format!("{what} (HKCU\\{path})"));
            }
        }
        if let Some(icon) = icon.filter(|icon| icon.exists()) {
            found.push(format!("the notification icon ({})", icon.display()));
        }
        found
    }

    /// Everything that was not there in the first place is not an error.
    pub fn remove(app_id: &str, icon: Option<&Path>) -> io::Result<()> {
        let user = RegKey::predef(HKEY_CURRENT_USER);
        for base in [ID_KEY, SETTINGS_KEY] {
            match user.delete_subkey_all(format!(r"{base}\{app_id}")) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        if let Some(icon) = icon {
            match fs::remove_file(icon) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn registers_lists_and_removes_everything() {
            let dir = tempfile::tempdir().unwrap();
            let icon = dir.path().join("icon.png");
            let app_id = format!("shalemore.outbound.test-{}", std::process::id());
            let path = format!(r"{ID_KEY}\{app_id}");
            let user = RegKey::predef(HKEY_CURRENT_USER);
            assert!(leftovers(&app_id, Some(&icon)).is_empty());

            write_icon(&icon).unwrap();
            register(&app_id, "Outbound test", Some(&icon)).unwrap();
            let key = user.open_subkey(&path).unwrap();
            assert_eq!(
                key.get_value::<String, _>("DisplayName").unwrap(),
                "Outbound test"
            );
            assert_eq!(
                key.get_value::<String, _>("IconUri").unwrap(),
                icon.to_string_lossy()
            );
            assert_eq!(key.get_value::<u32, _>("ShowInSettings").unwrap(), 1);
            assert_eq!(leftovers(&app_id, Some(&icon)).len(), 2);

            remove(&app_id, Some(&icon)).unwrap();
            assert!(user.open_subkey(&path).is_err());
            assert!(!icon.exists());
            assert!(leftovers(&app_id, Some(&icon)).is_empty());
            // Removing what is not there is not an error.
            remove(&app_id, Some(&icon)).unwrap();
        }
    }
}

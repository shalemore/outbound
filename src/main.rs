#![windows_subsystem = "windows"]

mod actions;
mod autostart;
mod cleanup;
mod config;
mod dialog;
mod hosts;
mod icon;
mod ledger;
mod notifier;
mod remote_dialog;
mod ssh;
mod status;
mod sync;
mod watch;
mod worker;

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process,
    sync::{Arc, Mutex, mpsc},
};

use anyhow::Result;
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tray_icon::{
    TrayIcon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu},
};

use crate::{
    actions::Notice,
    status::{IconState, Reporter, shorten},
    worker::{Command, Control},
};

const MAX_LOG_BYTES: u64 = 5 << 20;

enum UserEvent {
    Refresh,
    Menu(MenuEvent),
    Notify { title: String, body: String },
}

/// What the menu handlers need to reach the rest of the program.
struct Context {
    commands: mpsc::Sender<Command>,
    control: Arc<Control>,
    reporter: Reporter,
}

fn main() {
    // Run by hand or by the Scoop uninstaller before the program is removed. It needs neither the
    // single instance lock nor the log.
    if std::env::args().any(|argument| argument == "--cleanup") {
        process::exit(if cleanup::run().is_clean() { 0 } else { 1 });
    }
    if let Err(e) = run() {
        tracing::error!("exiting because of an unexpected error: {e:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let data_dir = config::data_dir()?;
    fs::create_dir_all(&data_dir)?;
    // Two running instances would upload everything twice, so only one is allowed. The lock is
    // released automatically when the process exits.
    let lock = File::create(data_dir.join("outbound.lock"))?;
    if lock.try_lock().is_err() {
        dialog::info("Outbound is already running. Its icon is in the notification area.");
        return Ok(());
    }
    init_logging(&data_dir.join("outbound.log"))?;
    autostart::refresh();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    let refresh_proxy = Mutex::new(event_loop.create_proxy());
    let reporter = Reporter::new(Arc::new(move || {
        if let Ok(proxy) = refresh_proxy.lock() {
            let _ = proxy.send_event(UserEvent::Refresh);
        }
    }));
    let (commands, command_receiver) = mpsc::channel();
    let control = Arc::new(Control::default());
    let notify_proxy = Mutex::new(event_loop.create_proxy());
    let notifier: worker::Notifier = Arc::new(move |title, body| {
        if let Ok(proxy) = notify_proxy.lock() {
            let _ = proxy.send_event(UserEvent::Notify {
                title: title.to_string(),
                body: body.to_string(),
            });
        }
    });
    worker::spawn(
        reporter.clone(),
        command_receiver,
        commands.clone(),
        notifier,
        Arc::clone(&control),
    );

    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    let context = Context {
        commands,
        control,
        reporter,
    };
    let mut app: Option<Tray> = None;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => match Tray::new(&context.reporter) {
                Ok(tray) => app = Some(tray),
                Err(e) => {
                    tracing::error!("cannot create the tray icon: {e:#}");
                    *control_flow = ControlFlow::Exit;
                }
            },
            Event::UserEvent(UserEvent::Refresh) => {
                if let Some(tray) = &mut app {
                    tray.refresh(&context.reporter);
                }
            }
            Event::UserEvent(UserEvent::Menu(event)) => {
                if let Some(tray) = &mut app
                    && tray.handle_menu(&event.id, &context)
                {
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Notify { title, body }) => notifier::send(&title, &body),
            _ => {}
        }
    })
}

struct Tray {
    icon: TrayIcon,
    menu: Menu,
    /// What is going on, and the details of it. The details are only in the menu while there is
    /// something to say.
    headline: [MenuItem; 2],
    details_shown: bool,
    /// Lists the conflicts and the problems. It is disabled while there are none.
    attention_menu: Submenu,
    /// The problems and the conflicts that are currently listed in the submenu.
    attention: (Vec<String>, Vec<String>),
    pause_resume: MenuItem,
    sync_now: MenuItem,
    host_menu: Submenu,
    /// The hosts of ~/.ssh/config that are currently listed in the submenu.
    hosts: Vec<(CheckMenuItem, String)>,
    reload_hosts: MenuItem,
    folder_menu: Submenu,
    /// The configured folders that are currently listed in the submenu, by local path.
    folders: Vec<(CheckMenuItem, String)>,
    reload_folders: MenuItem,
    remove_menu: Submenu,
    /// The configured folders that the submenu can remove, by local path and remote folder.
    removable: Vec<(MenuItem, String, String)>,
    add_mapping: MenuItem,
    autostart: CheckMenuItem,
    open_config: MenuItem,
    open_logs: MenuItem,
    clean_up: MenuItem,
    quit: MenuItem,
    /// The icon drawn last. It is not redrawn while the state and the progress percentage stay
    /// the same.
    drawn: Option<(IconState, Option<u32>)>,
}

impl Tray {
    fn new(reporter: &Reporter) -> Result<Self> {
        let headline = [(); 2].map(|()| MenuItem::new("", false, None));
        let attention_menu = Submenu::new("Problems", false);
        let pause_resume = MenuItem::new("Pause", true, None);
        let sync_now = MenuItem::new("Sync now", true, None);
        let host_menu = Submenu::new("SSH host", true);
        let reload_hosts = MenuItem::new("Reload hosts", true, None);
        let folder_menu = Submenu::new("Folders", true);
        let reload_folders = MenuItem::new("Reload folders", true, None);
        let remove_menu = Submenu::new("Remove folder", false);
        let add_mapping = MenuItem::new("Add folder…", true, None);
        let autostart =
            CheckMenuItem::new("Start with Windows", true, autostart::is_enabled(), None);
        let open_config = MenuItem::new("Open config file", true, None);
        let open_logs = MenuItem::new("Open log folder", true, None);
        let clean_up = MenuItem::new("Clean up and quit", true, None);
        let quit = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        let menu_handle = menu.clone();
        for line in &headline {
            menu.append(line)?;
        }
        menu.append(&attention_menu)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&pause_resume)?;
        menu.append(&sync_now)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&host_menu)?;
        menu.append(&folder_menu)?;
        menu.append(&autostart)?;
        menu.append(&open_config)?;
        menu.append(&open_logs)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&clean_up)?;
        menu.append(&quit)?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon::render(IconState::Idle, None)?)
            .build()?;
        let mut tray = Self {
            icon,
            menu: menu_handle,
            headline,
            details_shown: true,
            attention_menu,
            attention: Default::default(),
            pause_resume,
            sync_now,
            host_menu,
            hosts: Vec::new(),
            reload_hosts,
            folder_menu,
            folders: Vec::new(),
            reload_folders,
            remove_menu,
            removable: Vec::new(),
            add_mapping,
            autostart,
            open_config,
            open_logs,
            clean_up,
            quit,
            drawn: None,
        };
        tray.load_lists();
        tray.refresh(reporter);
        Ok(tray)
    }

    /// Fills the SSH host and the folder submenus from ~/.ssh/config and the configuration file.
    fn load_lists(&mut self) {
        let path = config::config_path().ok();

        while self.host_menu.remove_at(0).is_some() {}
        self.hosts.clear();
        let current = path.as_deref().and_then(config::current_destination);
        for host in hosts::ssh_hosts() {
            let checked = current.as_deref() == Some(host.as_str());
            let item = CheckMenuItem::new(&host, true, checked, None);
            let _ = self.host_menu.append(&item);
            self.hosts.push((item, host));
        }
        if self.hosts.is_empty() {
            let none = MenuItem::new("No hosts", false, None);
            let _ = self.host_menu.append(&none);
        }
        let _ = self.host_menu.append(&PredefinedMenuItem::separator());
        let _ = self.host_menu.append(&self.reload_hosts);
        let title = match current {
            Some(destination) => format!("SSH host: {destination}"),
            None => "SSH host".to_string(),
        };
        self.host_menu.set_text(title);

        while self.folder_menu.remove_at(0).is_some() {}
        self.folders.clear();
        while self.remove_menu.remove_at(0).is_some() {}
        self.removable.clear();
        let mappings = path
            .as_deref()
            .map(config::mappings_overview)
            .unwrap_or_default();
        for mapping in &mappings {
            let label = format!("{} → {}", mapping.local, mapping.remote);
            let item = CheckMenuItem::new(&label, true, mapping.enabled, None);
            let _ = self.folder_menu.append(&item);
            self.folders.push((item, mapping.local.clone()));
            let item = MenuItem::new(&label, true, None);
            let _ = self.remove_menu.append(&item);
            self.removable
                .push((item, mapping.local.clone(), mapping.remote.clone()));
        }
        if mappings.is_empty() {
            let none = MenuItem::new("No folders", false, None);
            let _ = self.folder_menu.append(&none);
        }
        let _ = self.folder_menu.append(&PredefinedMenuItem::separator());
        self.remove_menu.set_enabled(!mappings.is_empty());
        let _ = self.folder_menu.append(&self.remove_menu);
        let _ = self.folder_menu.append(&self.add_mapping);
        let _ = self.folder_menu.append(&self.reload_folders);
    }

    fn refresh(&mut self, reporter: &Reporter) {
        let status = reporter.snapshot();
        // A menu that is open is not changed under the mouse: only what differs is touched.
        let (state, details) = status.headline();
        set_text(&self.headline[0], state);
        match details {
            Some(details) => {
                set_text(&self.headline[1], details);
                if !self.details_shown {
                    let _ = self.menu.insert(&self.headline[1], 1);
                    self.details_shown = true;
                }
            }
            None if self.details_shown => {
                let _ = self.menu.remove(&self.headline[1]);
                self.details_shown = false;
            }
            None => {}
        }
        self.refresh_attention(&status);
        set_text(
            &self.pause_resume,
            if status.paused { "Resume" } else { "Pause" }.to_string(),
        );
        if self.sync_now.is_enabled() == status.paused {
            self.sync_now.set_enabled(!status.paused);
        }
        let _ = self.icon.set_tooltip(Some(status.tooltip()));

        let progress = status.transfer.as_ref().map(|t| t.fraction());
        let drawn = (status.icon_state(), progress.map(|p| (p * 100.0) as u32));
        if self.drawn != Some(drawn) {
            self.drawn = Some(drawn);
            match icon::render(drawn.0, progress) {
                Ok(icon) => {
                    let _ = self.icon.set_icon(Some(icon));
                }
                Err(e) => tracing::warn!("cannot draw the tray icon: {e:#}"),
            }
        }
    }

    fn refresh_attention(&mut self, status: &status::Status) {
        let lists = (status.problems.clone(), status.conflicts.clone());
        let title = status.attention_title();
        if self.attention_menu.text() != title {
            self.attention_menu.set_text(title);
        }
        if self.attention_menu.is_enabled() != status.needs_attention() {
            self.attention_menu.set_enabled(status.needs_attention());
        }
        if lists == self.attention {
            return;
        }
        while self.attention_menu.remove_at(0).is_some() {}
        for (label, items) in [("Problems", &lists.0), ("Conflicts", &lists.1)] {
            if items.is_empty() {
                continue;
            }
            let _ = self.attention_menu.append(&MenuItem::new(
                format!("{label} ({})", items.len()),
                false,
                None,
            ));
            for item in items {
                let _ = self.attention_menu.append(&MenuItem::new(
                    shorten(item, MAX_PROBLEM),
                    false,
                    None,
                ));
            }
        }
        self.attention = lists;
    }

    /// Returns true when the program should quit.
    fn handle_menu(&mut self, id: &MenuId, context: &Context) -> bool {
        if id == self.pause_resume.id() {
            let paused = !context.control.paused();
            context.control.set_paused(paused);
            context.reporter.update(|status| status.paused = paused);
            let _ = context.commands.send(Command::Wake);
        } else if id == self.sync_now.id() {
            let _ = context.commands.send(Command::SyncNow);
        } else if id == self.reload_hosts.id() || id == self.reload_folders.id() {
            self.load_lists();
        } else if let Some((_, host)) = self.hosts.iter().find(|(item, _)| item.id() == id) {
            match actions::use_host(host) {
                Ok(()) => {
                    let _ = context.commands.send(Command::SyncNow);
                }
                Err(e) => Notice::Error(format!("{e:#}")).show(),
            }
            // The check mark of the clicked item toggled by itself, so draw all of them again.
            self.load_lists();
        } else if let Some((item, local)) = self.folders.iter().find(|(item, _)| item.id() == id) {
            // The item has already toggled itself, so its state is the one that was asked for.
            match actions::set_folder_enabled(local, item.is_checked()) {
                Ok(()) => {
                    let _ = context.commands.send(Command::SyncNow);
                }
                Err(e) => Notice::Error(format!("{e:#}")).show(),
            }
            self.load_lists();
        } else if let Some((_, local, remote)) =
            self.removable.iter().find(|(item, ..)| item.id() == id)
        {
            let details = format!("It stops being sent to {remote}. No file is deleted.");
            if dialog::confirm(&format!("Remove {local}?"), &details, "Remove") {
                match actions::remove_folder(local) {
                    Ok(()) => {
                        let _ = context.commands.send(Command::SyncNow);
                    }
                    Err(e) => Notice::Error(format!("{e:#}")).show(),
                }
                self.load_lists();
            }
        } else if id == self.add_mapping.id() {
            actions::add_folder(&context.commands);
            self.load_lists();
        } else if id == self.autostart.id() {
            // The item has already toggled itself, so its state is the one that was asked for.
            if let Err(e) = autostart::set(self.autostart.is_checked()) {
                Notice::Error(format!("cannot change the startup entry: {e}")).show();
                self.autostart.set_checked(autostart::is_enabled());
            }
        } else if id == self.open_config.id() {
            open(config::config_path());
        } else if id == self.open_logs.id() {
            open(config::data_dir());
        } else if id == self.clean_up.id() {
            return clean_up_and_quit();
        } else if id == self.quit.id() {
            return true;
        }
        false
    }
}

/// The longest problem or conflict shown in a menu row. The whole text is in the log.
const MAX_PROBLEM: usize = 80;

fn set_text(item: &MenuItem, text: String) {
    if item.text() != text {
        item.set_text(text);
    }
}

/// Asks, removes what the program has put on this machine, checks it and only then quits. The
/// program file stays where it is.
fn clean_up_and_quit() -> bool {
    if !dialog::confirm(
        "Remove the notification entries and quit?",
        "Do this before you delete the program. Your configuration and log are kept.",
        "Clean up",
    ) {
        return false;
    }
    let outcome = cleanup::run();
    if outcome.is_clean() {
        dialog::info(&format!(
            "{}\n\nOutbound will quit now. The program file was not deleted.",
            outcome.describe()
        ));
        true
    } else {
        dialog::error(&format!(
            "{}\n\nOutbound keeps running, with notifications turned off.",
            outcome.describe()
        ));
        false
    }
}

fn open(path: Result<PathBuf>) {
    if let Err(e) = path.and_then(|path| open_in_explorer_or_notepad(&path)) {
        tracing::warn!("cannot open the file or folder: {e:#}");
    }
}

/// Windows has no program associated with .toml files by default, so the configuration opens in
/// Notepad instead of the "how do you want to open this file" dialog. A folder opens in Explorer.
fn open_in_explorer_or_notepad(path: &Path) -> Result<()> {
    let program = if path.is_dir() { "explorer" } else { "notepad" };
    process::Command::new(program).arg(path).spawn()?;
    Ok(())
}

/// When the log grows past the limit it is truncated, so it cannot grow without bound.
fn init_logging(path: &Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create(true);
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    let file = options.open(path)?;
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .init();
    Ok(())
}

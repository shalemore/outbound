//! The window that asks for a folder on the server. It is a plain Win32 window with the controls
//! and the font of the system, so that it looks like the rest of Windows. It opens at once and
//! suggests the home folder plus the name of the local folder when the server has answered. While
//! the path is typed it checks that the folder exists, Tab completes a name as in a shell, and the
//! list shows the folders next to it, so that a path can also be clicked together.

use std::{ffi::c_void, mem, path::Path, sync::Arc, thread};

use anyhow::Result;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{COLOR_BTNFACE, CreateFontIndirectW, DeleteObject, HBRUSH, HFONT, HGDIOBJ},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi},
            Input::KeyboardAndMouse::{
                EnableWindow, GetKeyState, IsWindowEnabled, SetFocus, VK_DOWN, VK_ESCAPE,
                VK_LBUTTON, VK_RETURN, VK_SHIFT, VK_TAB,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, w},
};

use crate::ssh::{Browse, SshRemote};

/// What the dialog needs to know from the server. Every call runs on a thread of its own.
pub trait Folders: Send + Sync {
    fn home(&self) -> Result<String>;
    fn browse(&self, path: &str) -> Result<Browse>;
}

impl Folders for SshRemote {
    fn home(&self) -> Result<String> {
        self.home_dir()
    }

    fn browse(&self, path: &str) -> Result<Browse> {
        SshRemote::browse(self, path)
    }
}

/// Window styles and messages of the controls, which live in a part of the Windows API that this
/// program has no other use for.
const SS_PATHELLIPSIS: u32 = 0x8000;
const EM_SETSEL: u32 = 0x00B1;

const ID_EDIT: i32 = 101;
const ID_STATUS: i32 = 102;
const ID_LIST_LABEL: i32 = 103;
const ID_LIST: i32 = 104;
const ID_OK: i32 = 1;
const ID_CANCEL: i32 = 2;

const TIMER_LOOKUP: usize = 1;
/// How long the typing has to pause before the server is asked.
const LOOKUP_DELAY_MS: u32 = 250;
/// An answer about a path. `wparam` is the number of the question and `lparam` a boxed
/// `Result<Browse>`.
const WM_ANSWER: u32 = WM_APP + 1;
/// The home folder of the server. `lparam` is a boxed `Result<String>`.
const WM_HOME: u32 = WM_APP + 2;

/// Everything the window procedure needs; the window keeps it behind GWLP_USERDATA.
struct State {
    folders: Arc<dyn Folders>,
    local: String,
    /// The name of the local folder, which the suggestion ends with.
    name: String,
    font: HFONT,
    edit: HWND,
    status: HWND,
    list_label: HWND,
    list: HWND,
    ok: HWND,
    /// The number of the latest question. An answer to an earlier one is out of date.
    question: usize,
    /// The server is asked one thing at a time, because its answers queue up behind each other.
    /// True from the moment a question is sent until its answer arrives.
    in_flight: bool,
    /// The typing paused while a question was in flight, so the text has not been asked about.
    lookup_due: bool,
    /// The text is being checked, and OK is disabled until the answer is in.
    checking: bool,
    /// OK was pressed while the text was being checked. The dialog closes if the answer is yes.
    submit_when_checked: bool,
    /// The latest answer, and the paths that the rows of the list open.
    browse: Option<Browse>,
    choices: Vec<String>,
    result: Option<String>,
}

/// Asks for the folder on the server that receives `local`. None means that the user cancelled.
pub fn ask(local: &Path, folders: Arc<dyn Folders>) -> Option<String> {
    let mut state = Box::new(State {
        folders,
        local: local.display().to_string(),
        name: local
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        font: HFONT::default(),
        edit: HWND::default(),
        status: HWND::default(),
        list_label: HWND::default(),
        list: HWND::default(),
        ok: HWND::default(),
        question: 0,
        in_flight: false,
        lookup_due: false,
        checking: false,
        submit_when_checked: false,
        browse: None,
        choices: Vec::new(),
        result: None,
    });
    let state: *mut State = &mut *state as *mut State;
    // SAFETY: the window and its message loop live inside this call, and the state outlives
    // them. The state is only reached through the pointer from here on.
    unsafe {
        run(state);
        let state = Box::from_raw(state);
        if !state.font.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(state.font.0));
        }
        state.result.filter(|text| !text.is_empty())
    }
}

unsafe fn run(state: *mut State) {
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return;
        };
        let class = w!("OutboundRemoteFolder");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module.into(),
            hIcon: LoadIconW(Some(module.into()), PCWSTR(std::ptr::without_provenance(1)))
                .unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut c_void),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&window_class);

        let Ok(window) = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class,
            w!("Outbound"),
            WS_CAPTION | WS_SYSMENU,
            0,
            0,
            100,
            100,
            None,
            None,
            Some(module.into()),
            Some(state as *const c_void),
        ) else {
            return;
        };
        let (edit, list) = ((*state).edit, (*state).list);
        let _ = ShowWindow(window, SW_SHOW);
        let _ = SetForegroundWindow(window);
        let _ = SetFocus(Some(edit));

        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            if message.message == WM_KEYDOWN && key(&*state, window, &message, edit, list) {
                continue;
            }
            if !IsDialogMessageW(window, &message).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let _ = UnregisterClassW(class, Some(module.into()));
    }
}

/// The keys that do something special in the box and in the list. Returns true when the key was
/// used up.
unsafe fn key(state: &State, window: HWND, message: &MSG, edit: HWND, list: HWND) -> bool {
    let key = message.wParam.0 as u16;
    let command = |id: i32| unsafe {
        SendMessageW(window, WM_COMMAND, Some(WPARAM(id as usize)), None);
        true
    };
    unsafe {
        match (message.hwnd == edit, message.hwnd == list) {
            (true, _) if key == VK_RETURN.0 => command(ID_OK),
            (_, true) if key == VK_RETURN.0 => open_selected(state, edit, list),
            (true, _) | (_, true) if key == VK_ESCAPE.0 => command(ID_CANCEL),
            (true, _) if key == VK_TAB.0 && GetKeyState(VK_SHIFT.0 as i32) >= 0 => {
                // Without anything to complete, Tab moves on to the next control as usual.
                let completed = state
                    .browse
                    .as_ref()
                    .and_then(|browse| browse.complete(&text_of(edit)));
                match completed {
                    Some(path) => {
                        put_path(edit, &path);
                        true
                    }
                    None => false,
                }
            }
            (true, _) if key == VK_DOWN.0 && !state.choices.is_empty() => {
                SendMessageW(list, LB_SETCURSEL, Some(WPARAM(0)), None);
                let _ = SetFocus(Some(list));
                true
            }
            _ => false,
        }
    }
}

/// Puts the path of the selected row into the box, which lists that folder next.
unsafe fn open_selected(state: &State, edit: HWND, list: HWND) -> bool {
    unsafe {
        let index = SendMessageW(list, LB_GETCURSEL, None, None).0;
        if let Some(path) = usize::try_from(index)
            .ok()
            .and_then(|index| state.choices.get(index))
        {
            put_path(edit, path);
            let _ = SetFocus(Some(edit));
        }
        true
    }
}

/// Replaces the text of the box and puts the caret at its end.
unsafe fn put_path(edit: HWND, path: &str) {
    unsafe {
        set_text(edit, path);
        let end = path.encode_utf16().count();
        SendMessageW(
            edit,
            EM_SETSEL,
            Some(WPARAM(end)),
            Some(LPARAM(end as isize)),
        );
    }
}

/// Scales a length in device independent pixels.
fn scale(length: i32, dpi: u32) -> i32 {
    (length * dpi as i32 + 48) / 96
}

unsafe fn state_of<'a>(window: HWND) -> Option<&'a mut State> {
    // SAFETY: the pointer is set in WM_CREATE to the state that outlives the window.
    unsafe { (GetWindowLongPtrW(window, GWLP_USERDATA) as *mut State).as_mut() }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_CREATE => {
                let create = &*(lparam.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
                if let Some(state) = state_of(window) {
                    build(window, state);
                    ask_for_home(window, state);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                if let Some(state) = state_of(window) {
                    command(window, state, wparam, lparam);
                }
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == TIMER_LOOKUP => {
                let _ = KillTimer(Some(window), TIMER_LOOKUP);
                if let Some(state) = state_of(window) {
                    ask_about_path(window, state);
                }
                LRESULT(0)
            }
            WM_HOME => {
                // The box is always taken back.
                let home = *Box::from_raw(lparam.0 as *mut Result<String>);
                if let Some(state) = state_of(window) {
                    show_home(state, home);
                }
                LRESULT(0)
            }
            WM_ANSWER => {
                // The box is always taken back, also when the answer is out of date.
                let answer = *Box::from_raw(lparam.0 as *mut Result<Browse>);
                if let Some(state) = state_of(window) {
                    state.in_flight = false;
                    if wparam.0 == state.question {
                        show_answer(state, answer);
                        if state.submit_when_checked && IsWindowEnabled(state.ok).as_bool() {
                            submit(window, state);
                        }
                    } else if mem::take(&mut state.lookup_due) {
                        ask_about_path(window, state);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(window);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

/// Makes the controls, scales the window to the screen it is on and puts it in the middle of it.
unsafe fn build(window: HWND, state: &mut State) {
    unsafe {
        let dpi = GetDpiForWindow(window).max(96);
        let metrics = {
            let mut metrics = NONCLIENTMETRICSW {
                cbSize: mem::size_of::<NONCLIENTMETRICSW>() as u32,
                ..Default::default()
            };
            let _ = SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS.0,
                metrics.cbSize,
                Some(&mut metrics as *mut _ as *mut c_void),
                0,
                dpi,
            );
            metrics
        };
        state.font = CreateFontIndirectW(&metrics.lfMessageFont);

        let child = |class: PCWSTR,
                     text: &str,
                     id: i32,
                     style: WINDOW_STYLE,
                     ex_style: WINDOW_EX_STYLE,
                     rect: (i32, i32, i32, i32)| {
            let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            let control = CreateWindowExW(
                ex_style,
                class,
                PCWSTR(text.as_ptr()),
                WS_CHILD | WS_VISIBLE | style,
                scale(rect.0, dpi),
                scale(rect.1, dpi),
                scale(rect.2, dpi),
                scale(rect.3, dpi),
                Some(window),
                Some(HMENU(id as usize as *mut c_void)),
                None,
                None,
            )
            .unwrap_or_default();
            SendMessageW(
                control,
                WM_SETFONT,
                Some(WPARAM(state.font.0 as usize)),
                Some(LPARAM(1)),
            );
            control
        };

        let none = WINDOW_EX_STYLE(0);
        let ellipsis = WINDOW_STYLE(SS_PATHELLIPSIS);
        child(
            w!("STATIC"),
            &format!("Folder on the server for {}", state.local),
            0,
            ellipsis,
            none,
            (16, 16, 408, 18),
        );
        state.edit = child(
            w!("EDIT"),
            "",
            ID_EDIT,
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            WS_EX_CLIENTEDGE,
            (16, 40, 408, 24),
        );
        state.status = child(
            w!("STATIC"),
            "Connecting…",
            ID_STATUS,
            ellipsis,
            none,
            (16, 70, 408, 18),
        );
        state.list_label = child(
            w!("STATIC"),
            "",
            ID_LIST_LABEL,
            ellipsis,
            none,
            (16, 100, 408, 18),
        );
        state.list = child(
            w!("LISTBOX"),
            "",
            ID_LIST,
            WS_TABSTOP | WS_VSCROLL | WINDOW_STYLE(LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32),
            WS_EX_CLIENTEDGE,
            (16, 120, 408, 120),
        );
        child(
            w!("STATIC"),
            "Tab completes a name.",
            0,
            WINDOW_STYLE(0),
            none,
            (16, 261, 216, 18),
        );
        state.ok = child(
            w!("BUTTON"),
            "OK",
            ID_OK,
            WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            none,
            (240, 256, 88, 26),
        );
        child(
            w!("BUTTON"),
            "Cancel",
            ID_CANCEL,
            WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            none,
            (336, 256, 88, 26),
        );
        let _ = EnableWindow(state.ok, false);

        // The size of the whole window that has the wanted client area.
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: scale(440, dpi),
            bottom: scale(298, dpi),
        };
        let _ = AdjustWindowRectExForDpi(&mut frame, WS_CAPTION | WS_SYSMENU, false, none, dpi);
        let (width, height) = (frame.right - frame.left, frame.bottom - frame.top);
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let _ = SetWindowPos(
            window,
            None,
            work.left + (work.right - work.left - width) / 2,
            work.top + (work.bottom - work.top - height) / 2,
            width,
            height,
            SWP_NOZORDER,
        );
    }
}

unsafe fn command(window: HWND, state: &mut State, wparam: WPARAM, lparam: LPARAM) {
    unsafe {
        let id = (wparam.0 & 0xffff) as i32;
        let notification = ((wparam.0 >> 16) & 0xffff) as u32;
        match id {
            ID_EDIT if notification == EN_CHANGE => {
                // Whatever was found for the earlier text no longer applies.
                state.question += 1;
                state.checking = true;
                state.submit_when_checked = false;
                let _ = EnableWindow(state.ok, false);
                set_text(state.status, "Checking…");
                let _ = SetTimer(Some(window), TIMER_LOOKUP, LOOKUP_DELAY_MS, None);
            }
            ID_LIST if notification == LBN_DBLCLK => {
                open_selected(state, state.edit, HWND(lparam.0 as *mut c_void));
            }
            ID_LIST if notification == LBN_SELCHANGE => {
                // Arrow keys also change the selection, and they only browse. A click opens the
                // folder.
                if GetKeyState(VK_LBUTTON.0 as i32) < 0 {
                    open_selected(state, state.edit, HWND(lparam.0 as *mut c_void));
                }
            }
            ID_OK => {
                if IsWindowEnabled(state.ok).as_bool() {
                    submit(window, state);
                } else if state.checking {
                    // Enter pressed before the server has answered is not lost.
                    state.submit_when_checked = true;
                }
            }
            ID_CANCEL => {
                let _ = DestroyWindow(window);
            }
            _ => {}
        }
    }
}

unsafe fn submit(window: HWND, state: &mut State) {
    unsafe {
        state.result = Some(text_of(state.edit).trim().to_string());
        let _ = DestroyWindow(window);
    }
}

/// Runs `ask` on a thread of its own and posts the boxed answer to the window as `message`.
fn post_answer<T: Send + 'static>(
    window: HWND,
    message: u32,
    wparam: usize,
    ask: impl FnOnce() -> T + Send + 'static,
) {
    let window = window.0 as isize;
    thread::spawn(move || {
        let answer = Box::into_raw(Box::new(ask()));
        // SAFETY: only a message is posted. When the window is gone, the box is taken back here.
        unsafe {
            let posted = PostMessageW(
                Some(HWND(window as *mut c_void)),
                message,
                WPARAM(wparam),
                LPARAM(answer as isize),
            );
            if posted.is_err() {
                drop(Box::from_raw(answer));
            }
        }
    });
}

fn ask_for_home(window: HWND, state: &State) {
    let folders = Arc::clone(&state.folders);
    post_answer(window, WM_HOME, 0, move || folders.home());
}

unsafe fn ask_about_path(window: HWND, state: &mut State) {
    let text = unsafe { text_of(state.edit) };
    let text = text.trim().to_string();
    if text.is_empty() {
        state.checking = false;
        unsafe { set_text(state.status, "") };
        return;
    }
    if state.in_flight {
        state.lookup_due = true;
        return;
    }
    state.in_flight = true;
    let folders = Arc::clone(&state.folders);
    post_answer(window, WM_ANSWER, state.question, move || {
        folders.browse(&text)
    });
}

/// Suggests the home folder plus the name of the local folder, unless something was typed while
/// the server was being reached.
fn show_home(state: &mut State, home: Result<String>) {
    unsafe {
        match home {
            Ok(home) if text_of(state.edit).is_empty() => {
                put_path(state.edit, &suggested(&home, &state.name));
            }
            Ok(_) => {}
            Err(e) => set_text(state.status, &format!("Cannot reach the server: {e:#}")),
        }
    }
}

fn show_answer(state: &mut State, answer: Result<Browse>) {
    unsafe {
        state.checking = false;
        let browse = match answer {
            Ok(browse) => browse,
            Err(e) => {
                set_text(state.status, &format!("Cannot check the folder: {e:#}"));
                return;
            }
        };
        let typed = text_of(state.edit);
        let name = typed.rsplit('/').next().unwrap_or_default();

        set_text(
            state.status,
            if browse.exists {
                "The folder exists."
            } else {
                "This folder does not exist on the server."
            },
        );
        let _ = EnableWindow(state.ok, browse.exists);

        set_text(state.list_label, &format!("Folders in {}", browse.listed));
        SendMessageW(state.list, LB_RESETCONTENT, None, None);
        state.choices.clear();
        let mut add = |label: &str, path: String| {
            let label: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
            SendMessageW(
                state.list,
                LB_ADDSTRING,
                None,
                Some(LPARAM(label.as_ptr() as isize)),
            );
            state.choices.push(path);
        };
        if let Some(parent) = browse.parent_path() {
            add("..", parent);
        }
        for folder in browse.matching(name) {
            add(folder, browse.path_of(folder));
        }
        state.browse = Some(browse);
    }
}

/// The home folder of the server plus the name of the local folder.
fn suggested(home: &str, name: &str) -> String {
    let home = home.trim_end_matches('/');
    if name.is_empty() {
        format!("{home}/")
    } else {
        format!("{home}/{name}")
    }
}

unsafe fn set_text(control: HWND, text: &str) {
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let _ = SetWindowTextW(control, PCWSTR(text.as_ptr()));
    }
}

unsafe fn text_of(control: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(control).max(0) as usize;
        let mut buffer = vec![0u16; length + 1];
        let copied = GetWindowTextW(control, &mut buffer).max(0) as usize;
        String::from_utf16_lossy(&buffer[..copied])
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn suggests_home_plus_the_folder_name() {
        assert_eq!(suggested("/home/me", "outbox"), "/home/me/outbox");
        assert_eq!(suggested("/root/", "x"), "/root/x");
        assert_eq!(suggested("/root", ""), "/root/");
    }

    /// Made-up answers from a server whose home is /data.
    struct Fake;

    impl Folders for Fake {
        fn home(&self) -> Result<String> {
            thread::sleep(Duration::from_millis(500));
            Ok("/data".into())
        }

        fn browse(&self, path: &str) -> Result<Browse> {
            thread::sleep(Duration::from_millis(100));
            let listed = path.rsplit_once('/').map_or("/", |(folder, _)| folder);
            Ok(Browse {
                exists: ["/data", "/data/videos", "/data/music"]
                    .contains(&path.trim_end_matches('/')),
                listed: if listed.is_empty() { "/" } else { listed }.into(),
                folders: ["videos", "music", "tv", ".cache"].map(String::from).into(),
            })
        }
    }

    /// Opens the dialog with made-up answers, to look at it. Run it with
    /// `cargo test --release show_the_dialog -- --ignored`.
    #[test]
    #[ignore]
    fn show_the_dialog() {
        let answer = ask(Path::new(r"D:\Downloads\outbox"), Arc::new(Fake));
        eprintln!("answer: {answer:?}");
    }
}

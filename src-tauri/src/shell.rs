//! Taskbar takeover: hides the Windows taskbar while Lucent runs and restores it
//! on exit, registers the dock as an app bar, places the dock, hides it for
//! fullscreen apps, maps the Windows key to the Lucent Start menu, and manages
//! autostart. See docs/taskbar-spec.md, module `shell`.
//!
//! Safety: the taskbar's state is saved to `%APPDATA%\Lucent\taskbar-state.json`
//! before anything changes, and `restore` puts it back. `restore` runs on exit,
//! from a panic hook, and from `--restore-taskbar`, which needs no running app.
//!
//! The dock is shown and hidden with raw `ShowWindow`, not tao, on purpose (see
//! island.rs). The Windows-key hook has a thread of its own whose message loop
//! does nothing else. The tray watchers run on a second thread. Tray windows are
//! shown and hidden with `ShowWindowAsync`, so no thread waits on Explorer. The
//! hook never blocks: it posts `start-key` to the main thread.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::io::Write;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, MutexGuard, Once};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, PhysicalSize, WebviewWindow};
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::UI::Accessibility::{
    SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
};
use windows::Win32::UI::Shell::{
    SHAppBarMessage, ABE_BOTTOM, ABM_GETSTATE, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS,
    ABM_SETSTATE, ABS_AUTOHIDE, APPBARDATA,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows,
    GetClassNameW, GetMessageW, GetWindowThreadProcessId, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetWindowsHookExW, ShowWindow, UnhookWindowsHookEx, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, MSG, SHOW_WINDOW_CMD, SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE, WH_KEYBOARD_LL,
    WINDOW_EX_STYLE, WINEVENT_OUTOFCONTEXT, WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP,
    WM_SETTINGCHANGE, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_POPUP, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND,
    OBJID_WINDOW, GetWindowRect, SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOOWNERZORDER,
    SPI_SETDESKWALLPAPER,
    SendMessageTimeoutW, ShowWindowAsync, EVENT_SYSTEM_DESKTOPSWITCH, SMTO_NORMAL, WM_NULL,
};

use crate::flyout;
use crate::glass;
use crate::island::{self, Screen};

pub const DOCK_LABEL: &str = "dock";

/// Dock height in logical px (DOCK_WINDOW_H in the spec).
const DOCK_HEIGHT: f64 = 104.0;
/// Work-area reservation at the bottom of the primary monitor, logical px.
const RESERVE_HEIGHT: f64 = 64.0;
/// Unassigned virtual key. Injected between a lone Win press and its release so
/// Windows does not treat the release as a request to open its Start menu.
const MASK_VK: u16 = 0xE8;
/// Win key virtual-key codes (VK_LWIN, VK_RWIN), as plain numbers for the pure code.
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const STATE_DIR: &str = "Lucent";
const STATE_FILE: &str = "taskbar-state.json";

// ---------------------------------------------------------------------------
// Process-wide state. Plain atomics are used wherever restore() may run from a
// panic hook, so restore never waits on a lock that a panicking thread holds.

/// Set once takeover has finished. Hooks and placement do nothing until then,
/// and restore clears it.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Dock HWND as an isize (0 = none). Used to remove the app bar on restore.
static DOCK_HWND: AtomicIsize = AtomicIsize::new(0);
/// Mirror of `DockState::suppressed` for the keyboard hook thread.
static SUPPRESSED: AtomicBool = AtomicBool::new(false);
/// `show_native_tray` is active: the tray is re-hidden on the next foreground change.
static NATIVE_TRAY: AtomicBool = AtomicBool::new(false);
/// Registered message id for "TaskbarCreated" (Explorer restarted).
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// Explorer's primary `Shell_TrayWnd` when takeover last ran. TaskbarCreated
/// with the same window is not a restart (Lucent's tray host broadcasts it too).
static EXPLORER_TRAY: AtomicIsize = AtomicIsize::new(0);
/// The shell thread's hidden watch window (0 = none) and that thread's id (0 = not started).
/// restore() uses them to wait for in-flight callbacks to finish.
static WATCH_HWND: AtomicIsize = AtomicIsize::new(0);
static SHELL_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// Bumped on every desktop switch. The keyboard hook drops its Win state when the
/// epoch it recorded is no longer current.
static DESKTOP_EPOCH: AtomicU32 = AtomicU32::new(0);

struct DockState {
    /// A fullscreen app covers the primary monitor, so the dock is hidden.
    suppressed: bool,
    /// Last placement applied: the dock rect and the app bar rect (left, top,
    /// right, bottom). Re-placing with the same values is skipped.
    placed: Option<(DockRect, [i32; 4])>,
}

static DOCK: Mutex<DockState> = Mutex::new(DockState {
    suppressed: false,
    placed: None,
});

fn dock_state() -> MutexGuard<'static, DockState> {
    DOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

thread_local! {
    /// App handle for the shell thread's callbacks. Only that thread reads it.
    static APP: RefCell<Option<AppHandle>> = const { RefCell::new(None) };
    /// Win key state for the keyboard hook, with the desktop epoch it belongs to.
    /// Only the keyboard thread touches it.
    static KEYS: Cell<(u32, WinKeys)> = Cell::new((0, WinKeys::default()));
}

// ---------------------------------------------------------------------------
// Pure: Windows key state machine.

/// Which Win keys are held, and whether the current press became a combo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WinKeys {
    lwin: bool,
    rwin: bool,
    /// Another key (or the other Win key) went down while Win was held.
    combo: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Let the event through untouched.
    Pass,
    /// A lone Win key was released. The hook injects the mask tap, then lets the
    /// release through, then emits `start-key`.
    MaskThenStart,
}

/// One keyboard event. Returns the new state and what the hook should do.
///
/// Injected events are ignored entirely, so Lucent's own SendInput (Win+N and
/// friends) and the mask tap never affect the state. Auto-repeat of a held Win
/// key is not a combo. Any other key pressed while Win is held makes the press a
/// combo, and a combo is never masked.
pub fn on_key(state: WinKeys, vk: u32, down: bool, injected: bool) -> (WinKeys, Action) {
    if injected {
        return (state, Action::Pass);
    }
    if vk != VK_LWIN && vk != VK_RWIN {
        if down && (state.lwin || state.rwin) {
            return (WinKeys { combo: true, ..state }, Action::Pass);
        }
        return (state, Action::Pass);
    }
    let is_left = vk == VK_LWIN;
    let (held, other_held) = if is_left {
        (state.lwin, state.rwin)
    } else {
        (state.rwin, state.lwin)
    };
    if down {
        if held {
            // Auto-repeat of the Win key already held.
            return (state, Action::Pass);
        }
        let mut next = if is_left {
            WinKeys { lwin: true, ..state }
        } else {
            WinKeys { rwin: true, ..state }
        };
        if other_held {
            next.combo = true;
        }
        return (next, Action::Pass);
    }
    if !held {
        // Release of a Win key we never saw go down.
        return (state, Action::Pass);
    }
    let next = if is_left {
        WinKeys { lwin: false, ..state }
    } else {
        WinKeys { rwin: false, ..state }
    };
    if next.lwin || next.rwin {
        // The other Win key is still held, so the press continues.
        return (next, Action::Pass);
    }
    let action = if state.combo {
        Action::Pass
    } else {
        Action::MaskThenStart
    };
    (WinKeys::default(), action)
}

/// The Win key state to use at `epoch`. A state recorded under an older epoch
/// (a desktop switch happened since) is dropped, so a stale combo cannot survive
/// a switch to the lock screen or another desktop.
pub fn keys_for_epoch(stored: (u32, WinKeys), epoch: u32) -> WinKeys {
    if stored.0 == epoch {
        stored.1
    } else {
        WinKeys::default()
    }
}

// ---------------------------------------------------------------------------
// Pure: geometry.

/// The dock window's rect in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DockRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Dock window: the monitor's full width, 104 logical px tall, bottom-aligned.
pub fn dock_rect(screen: Screen) -> DockRect {
    let height = (DOCK_HEIGHT * screen.scale).round() as u32;
    DockRect {
        x: screen.x,
        y: screen.y + screen.height as i32 - height as i32,
        width: screen.width,
        height,
    }
}

/// App bar reservation: the bottom 64 logical px of the monitor, in physical px.
pub fn reserve_rect(screen: Screen) -> RECT {
    let height = (RESERVE_HEIGHT * screen.scale).round() as i32;
    let bottom = screen.y + screen.height as i32;
    RECT {
        left: screen.x,
        top: bottom - height,
        right: screen.x + screen.width as i32,
        bottom,
    }
}

fn rect_key(rect: RECT) -> [i32; 4] {
    [rect.left, rect.top, rect.right, rect.bottom]
}

// ---------------------------------------------------------------------------
// Pure: saved taskbar state, autostart value and tray classes.

/// The taskbar state saved before takeover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedState {
    pub autohide: bool,
    /// Raw ABM_GETSTATE value, restored exactly. Missing in hand-written files.
    #[serde(default)]
    pub raw: Option<u32>,
}

impl SavedState {
    pub fn from_raw(raw: u32) -> Self {
        Self {
            autohide: (raw & ABS_AUTOHIDE) != 0,
            raw: Some(raw),
        }
    }

    /// The ABM_SETSTATE value that puts the taskbar back as it was.
    pub fn state_value(&self) -> u32 {
        self.raw.unwrap_or(if self.autohide { ABS_AUTOHIDE } else { 0 })
    }
}

pub fn parse_state(text: &str) -> Result<SavedState, String> {
    serde_json::from_str(text).map_err(|e| e.to_string())
}

pub fn encode_state(state: &SavedState) -> Result<String, String> {
    serde_json::to_string(state).map_err(|e| e.to_string())
}

/// The Run value Lucent writes: the exe path in quotes.
pub fn run_command(exe: &str) -> String {
    format!("\"{exe}\"")
}

/// True when the stored Run value points at `exe`. Quotes are optional and the
/// comparison ignores case, since Windows paths are case-insensitive.
pub fn run_value_matches(stored: &str, exe: &str) -> bool {
    unquote(stored).eq_ignore_ascii_case(unquote(exe))
}

fn unquote(text: &str) -> &str {
    text.trim().trim_start_matches('"').trim_end_matches('"')
}

/// Classes of the taskbar windows that takeover hides and restore shows.
pub fn is_shell_tray_class(name: &str) -> bool {
    matches!(name, "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
}

/// Classes that belong to the native tray, including the overflow flyout. A
/// foreground change to one of these keeps a `show_native_tray` session open.
pub fn is_native_tray_class(name: &str) -> bool {
    is_shell_tray_class(name)
        || matches!(
            name,
            "NotifyIconOverflowWindow"
                | "TopLevelWindowForOverflowXamlIsland"
                | "Xaml_WindowedPopupClass"
        )
}

// ---------------------------------------------------------------------------
// Takeover and restore.

/// Hides the Windows taskbar, registers and places the dock, shows it, installs
/// the Windows-key hook and the restore safety nets. On failure the taskbar is
/// restored before the error is returned.
pub fn takeover(app: &AppHandle) -> Result<(), String> {
    if ACTIVE.load(Ordering::SeqCst) {
        return Ok(());
    }
    install_panic_hook();
    match take_over(app) {
        Ok(()) => Ok(()),
        Err(err) => {
            restore_quiet();
            Err(err)
        }
    }
}

fn take_over(app: &AppHandle) -> Result<(), String> {
    // Nothing changes until Shell_TrayWnd is known to exist and the original
    // state is safely on disk.
    if !shell_tray_present() {
        return Err("Shell_TrayWnd was not found; the taskbar was left alone".into());
    }
    save_state_once()?;
    apply_state(get_state() | ABS_AUTOHIDE)?;
    EXPLORER_TRAY.store(primary_tray_bits(), Ordering::SeqCst);

    let window = dock_window(app)?;
    let hwnd = window.hwnd().map_err(|e| e.to_string())?;
    DOCK_HWND.store(hwnd_bits(hwnd), Ordering::SeqCst);
    if !register_appbar(hwnd) {
        return Err("the dock could not be registered as an app bar".into());
    }
    place_dock_inner(app)?;
    start_shell_threads(app.clone())?;
    ACTIVE.store(true, Ordering::SeqCst);
    // Explorer re-shows the auto-hide bar when the app bar changes, so hide it
    // only now that the app bar is registered and the dock is placed.
    set_trays_visible(false);
    show_dock_raw(&window, true)
}

/// Puts the Windows taskbar back exactly as it was. Safe to call more than once,
/// and needs no running Lucent process. Returns an error when the state file
/// had to be kept (it could not be read, or the taskbar did not take the saved
/// setting). The trays are shown in every case.
pub fn restore() -> Result<(), String> {
    ACTIVE.store(false, Ordering::SeqCst);
    NATIVE_TRAY.store(false, Ordering::SeqCst);
    // Let any callback already running on the shell thread finish before the
    // trays are shown, so it cannot hide them again.
    wait_for_shell_thread();
    let dock = DOCK_HWND.swap(0, Ordering::SeqCst);
    if dock != 0 {
        remove_appbar(hwnd_from_bits(dock));
    }
    let outcome = match read_state() {
        Ok(Some(saved)) => apply_state(saved.state_value())
            .map_err(|err| format!("{err}; keeping {}", state_path_text())),
        Ok(None) => Ok(()),
        Err(err) => Err(err),
    };
    set_trays_visible(true);
    if outcome.is_ok() {
        delete_state_file();
    }
    outcome
}

/// `restore` for the exit and panic paths, where there is nothing to return to.
/// The error is printed instead.
pub fn restore_quiet() {
    if let Err(err) = restore() {
        eprintln!("lucent: {err}");
    }
}

/// `lucent --restore-taskbar`: restore and exit. Exits 1 when the restore failed
/// (recovery after a crash or kill).
pub fn restore_cli() {
    if let Err(err) = restore() {
        eprintln!("lucent: {err}");
        std::process::exit(1);
    }
}

/// Blocks until the shell thread has handled a message sent to its watch window,
/// or 500 ms pass. Skipped on the shell thread itself, and when the thread is not running.
fn wait_for_shell_thread() {
    let watch = WATCH_HWND.load(Ordering::SeqCst);
    if watch == 0 {
        return;
    }
    // SAFETY: GetCurrentThreadId takes no arguments and cannot fail.
    let me = unsafe { GetCurrentThreadId() };
    if me == SHELL_THREAD_ID.load(Ordering::SeqCst) {
        return;
    }
    let mut result = 0usize;
    // SAFETY: the window is the shell thread's watch window. The timeout bounds the
    // wait, and result is a valid out-pointer.
    unsafe {
        SendMessageTimeoutW(
            hwnd_from_bits(watch),
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_NORMAL,
            500,
            Some(&mut result as *mut usize),
        )
    };
}

/// Hides the dock while a fullscreen app covers the primary monitor. Showing it
/// again places it first. Does nothing when the state is unchanged, or before
/// takeover has finished.
pub fn set_dock_suppressed(app: &AppHandle, suppressed: bool) {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    let window = match dock_window(app) {
        Ok(window) => window,
        Err(err) => {
            eprintln!("lucent: dock suppression failed: {err}");
            return;
        }
    };
    {
        let mut state = dock_state();
        if state.suppressed == suppressed {
            return;
        }
        state.suppressed = suppressed;
        SUPPRESSED.store(suppressed, Ordering::SeqCst);
    }
    let result = if suppressed {
        flyout::force_hide(app);
        show_dock_raw(&window, false)
    } else {
        place_dock_inner(app).and_then(|()| show_dock_raw(&window, true))
    };
    if let Err(err) = result {
        eprintln!("lucent: dock suppression failed: {err}");
    }
}

/// Re-places the dock (monitor or DPI change). Never shows a suppressed dock.
pub fn place_dock(app: &AppHandle) {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    place_dock_logged(app);
}

fn place_dock_logged(app: &AppHandle) {
    if let Err(err) = place_dock_inner(app) {
        eprintln!("lucent: dock placement failed: {err}");
    }
}

/// Places the dock window and its app bar reservation on the primary monitor.
/// Skips the work when the placement is unchanged.
fn place_dock_inner(app: &AppHandle) -> Result<(), String> {
    let window = dock_window(app)?;
    let hwnd = window.hwnd().map_err(|e| e.to_string())?;
    let monitor = window
        .primary_monitor()
        .map_err(|e| e.to_string())?
        .ok_or("no primary monitor")?;
    let screen = island::screen_of(&monitor);
    let rect = dock_rect(screen);
    let reserve = reserve_rect(screen);
    let key = (rect, rect_key(reserve));
    let mut state = dock_state();
    // Something else (a work-area change) can move the window, so the real
    // rect is checked too, not only the last request.
    if state.placed == Some(key) && window_rect(hwnd) == Some(rect) {
        return Ok(());
    }
    window
        .set_size(PhysicalSize::new(rect.width, rect.height))
        .map_err(|e| e.to_string())?;
    set_appbar_pos(hwnd, reserve);
    // Positioned after the reservation: the work-area change it causes must not
    // leave the dock above the reserved strip.
    // SAFETY: hwnd is the dock's live window handle; the call reads only its arguments.
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            rect.x,
            rect.y,
            rect.width as i32,
            rect.height as i32,
            SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
    }
    .map_err(|e| e.to_string())?;
    state.placed = Some(key);
    Ok(())
}

/// The window's current rect in physical px, as a DockRect.
fn window_rect(hwnd: HWND) -> Option<DockRect> {
    let mut rc = RECT::default();
    // SAFETY: rc is a valid, writable RECT for the duration of the call.
    unsafe { GetWindowRect(hwnd, &mut rc) }.ok()?;
    Some(DockRect {
        x: rc.left,
        y: rc.top,
        width: (rc.right - rc.left).max(0) as u32,
        height: (rc.bottom - rc.top).max(0) as u32,
    })
}

// ---------------------------------------------------------------------------
// Saved state file.

fn state_path() -> Result<PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or("APPDATA is not set")?;
    Ok(PathBuf::from(appdata).join(STATE_DIR).join(STATE_FILE))
}

fn state_path_text() -> String {
    state_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| STATE_FILE.to_string())
}

/// `Ok(None)` when there is no file. A zero-length file (left by a crash during a
/// write) counts as absent. Any other unreadable or corrupt file is an error, so
/// takeover never overwrites or ignores it.
fn read_state() -> Result<Option<SavedState>, String> {
    let path = state_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => parse_state(&text)
            .map(Some)
            .map_err(|e| format!("{} is unreadable ({e})", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{} cannot be read: {err}", path.display())),
    }
}

/// Saves the original state, but only when there is no saved state yet, so the
/// original is never overwritten. The text goes to a temporary file, is synced,
/// and is then renamed over the target, so a crash leaves either the old file or
/// the new one.
fn save_state_once() -> Result<(), String> {
    if read_state()?.is_some() {
        return Ok(());
    }
    let path = state_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_file_name(format!("{STATE_FILE}.tmp"));
    let text = encode_state(&SavedState::from_raw(get_state()))?;
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {err}", tmp.display()));
    }
    std::fs::rename(&tmp, &path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot move the state file into place: {err}")
    })
}

/// Removes the saved state and any leftover temporary file.
fn delete_state_file() {
    let Ok(path) = state_path() else {
        return;
    };
    for target in [path.clone(), path.with_file_name(format!("{STATE_FILE}.tmp"))] {
        if let Err(err) = std::fs::remove_file(&target) {
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("lucent: cannot delete {}: {err}", target.display());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Taskbar windows and the app bar.

fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_quiet();
            previous(info);
            std::process::exit(101);
        }));
    });
}

/// `ABM_GETSTATE`: the taskbar's ABS_* flags.
fn get_state() -> u32 {
    let mut data = appbar_data(HWND(std::ptr::null_mut()));
    // SAFETY: data is a live, correctly sized APPBARDATA. ABM_GETSTATE only reads it.
    unsafe { SHAppBarMessage(ABM_GETSTATE, &mut data) as u32 }
}

/// `ABM_SETSTATE`, then reads the state back to check the auto-hide bit took.
fn apply_state(value: u32) -> Result<(), String> {
    let mut data = appbar_data(HWND(std::ptr::null_mut()));
    data.lParam = LPARAM(value as isize);
    // SAFETY: data is a live APPBARDATA. ABM_SETSTATE reads lParam only.
    unsafe { SHAppBarMessage(ABM_SETSTATE, &mut data) };
    if get_state() & ABS_AUTOHIDE == value & ABS_AUTOHIDE {
        Ok(())
    } else {
        Err("the taskbar did not accept the auto-hide setting".into())
    }
}

fn appbar_data(hwnd: HWND) -> APPBARDATA {
    APPBARDATA {
        cbSize: size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        ..Default::default()
    }
}

/// `ABM_NEW` with a callback message. The dock's callbacks are ignored, so the
/// message only has to be unique.
fn register_appbar(hwnd: HWND) -> bool {
    // SAFETY: the message name is a static string literal.
    let callback = unsafe { RegisterWindowMessageW(w!("LucentAppBar")) };
    let mut data = APPBARDATA {
        uCallbackMessage: callback,
        ..appbar_data(hwnd)
    };
    // SAFETY: data is a live APPBARDATA. A nonzero return means registered.
    unsafe { SHAppBarMessage(ABM_NEW, &mut data) != 0 }
}

fn remove_appbar(hwnd: HWND) {
    let mut data = appbar_data(hwnd);
    // SAFETY: data is a live APPBARDATA. Removing a window that is not registered is harmless.
    unsafe { SHAppBarMessage(ABM_REMOVE, &mut data) };
}

/// Query then set, so the shell can adjust the rect against other app bars.
fn set_appbar_pos(hwnd: HWND, rc: RECT) {
    let mut data = APPBARDATA {
        uEdge: ABE_BOTTOM,
        rc,
        ..appbar_data(hwnd)
    };
    // SAFETY: data is a live APPBARDATA that both messages read and update.
    unsafe {
        SHAppBarMessage(ABM_QUERYPOS, &mut data);
        SHAppBarMessage(ABM_SETPOS, &mut data);
    }
}

/// Every shell tray window (`Shell_TrayWnd` and each `Shell_SecondaryTrayWnd`).
fn tray_windows() -> Vec<HWND> {
    let mut found: Vec<HWND> = Vec::new();
    // SAFETY: `found` outlives the synchronous EnumWindows call, and collect_trays
    // is the only code that dereferences the pointer during it.
    let _ = unsafe { EnumWindows(Some(collect_trays), LPARAM(&mut found as *mut Vec<HWND> as isize)) };
    found
}

unsafe extern "system" fn collect_trays(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the &mut Vec<HWND> that tray_windows passed in, and it is
    // alive for the whole enumeration.
    let found = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    // Lucent's own Shell_TrayWnd (the tray host in tray.rs) is not a taskbar.
    if class_name(hwnd).is_some_and(|name| is_shell_tray_class(&name)) && !is_ours(hwnd) {
        found.push(hwnd);
    }
    BOOL(1)
}

/// Shows or hides the shell tray windows. Hiding is what takeover does, and
/// showing is what restore does.
fn set_trays_visible(visible: bool) {
    let cmd = if visible { SW_SHOW } else { SW_HIDE };
    show_async(&tray_windows(), cmd);
}

/// Explorer's primary `Shell_TrayWnd`, as bits (0 when there is none).
fn primary_tray_bits() -> isize {
    tray_windows()
        .into_iter()
        .find(|hwnd| class_name(*hwnd).as_deref() == Some("Shell_TrayWnd"))
        .map_or(0, hwnd_bits)
}

/// True when a TaskbarCreated means Explorer made a new taskbar, rather than
/// someone (Lucent's tray host) asking apps to re-register.
pub fn is_new_taskbar(previous: isize, current: isize) -> bool {
    current != 0 && current != previous
}

/// True when a `Shell_TrayWnd` (the primary taskbar) exists.
fn shell_tray_present() -> bool {
    tray_windows()
        .into_iter()
        .any(|hwnd| class_name(hwnd).as_deref() == Some("Shell_TrayWnd"))
}

/// Shows or hides windows with ShowWindowAsync. The request is posted to the
/// owning thread and this returns at once, so an owner that is not pumping
/// messages (or Explorer, busy or hung) cannot block the caller.
fn show_async(hwnds: &[HWND], cmd: SHOW_WINDOW_CMD) {
    for hwnd in hwnds {
        // SAFETY: hwnd is a window handle taken from EnumWindows or an event. The
        // call only posts the request; its return value is not an error.
        let _ = unsafe { ShowWindowAsync(*hwnd, cmd) };
    }
}

fn class_name(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 256];
    // SAFETY: buf is a live buffer, and GetClassNameW writes at most its length.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    (len > 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

fn is_ours(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    // SAFETY: pid is a valid out-pointer for the call.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32)) };
    pid == std::process::id()
}

fn hwnd_bits(hwnd: HWND) -> isize {
    hwnd.0 as usize as isize
}

fn hwnd_from_bits(bits: isize) -> HWND {
    HWND(bits as usize as *mut c_void)
}

// ---------------------------------------------------------------------------
// The dock window.

fn dock_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window(DOCK_LABEL)
        .ok_or_else(|| "dock window not found".to_string())
}

/// Raw show or hide. tao keeps its own visibility flag, and this never updates
/// it, so tao's show() and hide() are never called on the dock.
fn show_dock_raw(window: &WebviewWindow, visible: bool) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|e| e.to_string())?;
    let cmd: SHOW_WINDOW_CMD = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
    // SAFETY: hwnd is the dock's live window handle. ShowWindow's return value is
    // the previous visibility, not an error.
    let _ = unsafe { ShowWindow(hwnd, cmd) };
    Ok(())
}

// ---------------------------------------------------------------------------
// Shell thread: Windows-key hook, native-tray watcher, Explorer-restart watcher.

fn start_shell_threads(app: AppHandle) -> Result<(), String> {
    let (keys_tx, keys_rx) = mpsc::channel::<Result<(), String>>();
    let keys_app = app.clone();
    std::thread::Builder::new()
        .name("lucent-keyboard".into())
        .spawn(move || keyboard_thread(keys_app, keys_tx))
        .map_err(|e| e.to_string())?;
    keys_rx.recv().map_err(|e| e.to_string())??;

    let (shell_tx, shell_rx) = mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("lucent-shell".into())
        .spawn(move || shell_thread(app, shell_tx))
        .map_err(|e| e.to_string())?;
    shell_rx.recv().map_err(|e| e.to_string())?
}

/// Keyboard thread. It owns the Windows-key hook and nothing else, so no other
/// work can delay the hook past the low-level hook timeout.
fn keyboard_thread(app: AppHandle, ready: Sender<Result<(), String>>) {
    APP.with(|slot| *slot.borrow_mut() = Some(app));
    let instance = match module_instance() {
        Ok(instance) => instance,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    // SAFETY: the callback is a static extern fn and the module handle is the
    // process's own. The hook is removed below when the loop ends.
    let keyboard = match unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard_proc), Some(instance), 0)
    } {
        Ok(hook) => hook,
        Err(err) => {
            let _ = ready.send(Err(format!("the Windows-key hook failed: {err}")));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    pump_messages();
    // SAFETY: keyboard was returned by SetWindowsHookExW above and is unhooked once.
    unsafe {
        let _ = UnhookWindowsHookEx(keyboard);
    }
}

/// Shell thread: the tray watchers, the Explorer-restart watcher and the desktop
/// switch watcher.
fn shell_thread(app: AppHandle, ready: Sender<Result<(), String>>) {
    APP.with(|slot| *slot.borrow_mut() = Some(app));
    // SAFETY: GetCurrentThreadId takes no arguments and cannot fail.
    SHELL_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let instance = match module_instance() {
        Ok(instance) => instance,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let watch = match create_shell_window(instance) {
        Ok(hwnd) => hwnd,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    WATCH_HWND.store(hwnd_bits(watch), Ordering::SeqCst);
    // SAFETY: the callback is a static extern fn, and the hook is out of context.
    let tray_show = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_SHOW,
            None,
            Some(on_tray_show),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if tray_show.0.is_null() {
        let _ = ready.send(Err("the taskbar re-hide hook failed".into()));
        return;
    }
    // SAFETY: the callback is a static extern fn, and the hook is out of context.
    let foreground = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if foreground.0.is_null() {
        eprintln!("lucent: native tray watcher unavailable");
    }
    // SAFETY: the callback is a static extern fn, and the hook is out of context.
    let desktop = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_DESKTOPSWITCH,
            EVENT_SYSTEM_DESKTOPSWITCH,
            None,
            Some(on_desktop_switch),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if desktop.0.is_null() {
        eprintln!("lucent: desktop switch watcher unavailable");
    }
    // SAFETY: the name is a static string literal.
    TASKBAR_CREATED.store(unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }, Ordering::SeqCst);
    let _ = ready.send(Ok(()));

    pump_messages();

    // SAFETY: each handle was returned by the calls above and is unhooked once.
    unsafe {
        let _ = UnhookWinEvent(tray_show);
        if !foreground.0.is_null() {
            let _ = UnhookWinEvent(foreground);
        }
        if !desktop.0.is_null() {
            let _ = UnhookWinEvent(desktop);
        }
    }
}

/// Runs a message loop until the thread is told to quit.
fn pump_messages() {
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-pointer. GetMessageW returns <= 0 on quit or error.
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if result <= 0 {
            break;
        }
        // SAFETY: msg was just filled in by GetMessageW.
        unsafe { DispatchMessageW(&msg) };
    }
}

fn module_instance() -> Result<HINSTANCE, String> {
    // SAFETY: a null name asks for the handle of the current executable.
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(|e| e.to_string())?;
    Ok(module.into())
}

/// Hidden popup, never shown. It receives TaskbarCreated, WM_DISPLAYCHANGE and
/// WM_SETTINGCHANGE on the shell thread. (Message-only windows miss the
/// broadcasts, so this is a popup like the one in fullscreen.rs.)
fn create_shell_window(instance: HINSTANCE) -> Result<HWND, String> {
    let class = w!("LucentShellWatch");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(shell_watch_proc),
        hInstance: instance,
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: wc and the class name are valid for the call. RegisterClassW returns
    // 0 when the class already exists, which is fine on the one thread that calls this.
    unsafe { RegisterClassW(&wc) };
    // SAFETY: the class is registered above, and every pointer argument is null or static.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|e| e.to_string())
}

unsafe extern "system" fn shell_watch_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let restarted = TASKBAR_CREATED.load(Ordering::SeqCst);
    if restarted != 0 && msg == restarted {
        post_to_main(explorer_restarted);
        return LRESULT(0);
    }
    match msg {
        WM_DISPLAYCHANGE => {
            post_to_main(on_display_or_settings_changed);
            post_to_main(glass::backdrop_changed);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            post_to_main(on_display_or_settings_changed);
            // wParam is the SPI action; the wallpaper changed when it is SPI_SETDESKWALLPAPER.
            if wparam.0 == SPI_SETDESKWALLPAPER.0 as usize {
                post_to_main(glass::backdrop_changed);
            }
            LRESULT(0)
        }
        // SAFETY: the arguments are the ones Windows passed to this window procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Runs `run` on the main thread with the app handle. Returns at once.
fn post_to_main(run: fn(&AppHandle)) {
    let app = APP.with(|slot| slot.borrow().clone());
    if let Some(app) = app {
        let main = app.clone();
        let _ = app.run_on_main_thread(move || run(&main));
    }
}

/// Display or settings changed (the work area may have moved): re-place the dock
/// and hide the tray again during takeover.
fn on_display_or_settings_changed(app: &AppHandle) {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    place_dock_logged(app);
    if !NATIVE_TRAY.load(Ordering::SeqCst) {
        set_trays_visible(false);
    }
}

/// Explorer restarted: its new tray is visible again and the auto-hide setting
/// was reset. Hide the tray, set auto-hide again, re-register and re-place the dock.
fn explorer_restarted(app: &AppHandle) {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    let current = primary_tray_bits();
    if !is_new_taskbar(EXPLORER_TRAY.load(Ordering::SeqCst), current) {
        // Re-applying auto-hide would make Explorer re-raise its tray above the
        // tray host while apps are re-registering.
        return;
    }
    EXPLORER_TRAY.store(current, Ordering::SeqCst);
    NATIVE_TRAY.store(false, Ordering::SeqCst);
    set_trays_visible(false);
    if let Err(err) = apply_state(get_state() | ABS_AUTOHIDE) {
        eprintln!("lucent: {err}");
    }
    flyout::force_hide(app);
    let bits = DOCK_HWND.load(Ordering::SeqCst);
    if bits != 0 {
        // A restarted shell may have forgotten the registration. Ignore a
        // refusal (the window may already be registered).
        register_appbar(hwnd_from_bits(bits));
    }
    dock_state().placed = None;
    place_dock_logged(app);
}

/// Keyboard hook. Must return fast: it only updates state, optionally injects
/// the mask tap, and posts `start-key` to the main thread.
unsafe extern "system" fn ll_keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && ACTIVE.load(Ordering::SeqCst) {
        // SAFETY: for WH_KEYBOARD_LL with code >= 0, lparam points at a KBDLLHOOKSTRUCT
        // that stays valid for the duration of this call.
        let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let message = wparam.0 as u32;
        let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let up = message == WM_KEYUP || message == WM_SYSKEYUP;
        if down || up {
            let injected = info.flags.contains(LLKHF_INJECTED);
            let epoch = DESKTOP_EPOCH.load(Ordering::SeqCst);
            let state = keys_for_epoch(KEYS.with(|keys| keys.get()), epoch);
            let (next, action) = on_key(state, info.vkCode, down, injected);
            KEYS.with(|keys| keys.set((epoch, next)));
            if action == Action::MaskThenStart {
                inject_mask();
                // While a fullscreen app holds the screen, the Start flyout would
                // steal its focus, so only the mask is sent.
                if !SUPPRESSED.load(Ordering::SeqCst) {
                    post_to_main(emit_start);
                }
            }
        }
    }
    // SAFETY: the arguments are the ones Windows passed to this hook procedure.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Injects the mask tap (down then up). The hook ignores injected input, so this
/// does not feed back into the state machine.
fn inject_mask() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(MASK_VK),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    // SAFETY: inputs holds two fully initialised INPUT structs, and the size is
    // that of one INPUT.
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

fn emit_start(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(DOCK_LABEL) {
        if let Err(err) = window.emit("start-key", ()) {
            eprintln!("lucent: start-key emit failed: {err}");
        }
    }
}

/// EVENT_OBJECT_SHOW: while takeover is active and no native-tray session is
/// open, a shell tray window that Explorer shows again is hidden straight away.
/// Explorer shows the auto-hide bar when the mouse reaches the bottom edge.
unsafe extern "system" fn on_tray_show(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 || hwnd.0.is_null() {
        return;
    }
    if !ACTIVE.load(Ordering::SeqCst) || NATIVE_TRAY.load(Ordering::SeqCst) {
        return;
    }
    if class_name(hwnd).is_some_and(|name| is_shell_tray_class(&name)) && !is_ours(hwnd) {
        show_async(&[hwnd], SW_HIDE);
    }
}

/// EVENT_SYSTEM_DESKTOPSWITCH: starts a new desktop epoch. The keyboard hook
/// drops its Win key state, so a combo cannot carry across a switch.
unsafe extern "system" fn on_desktop_switch(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    DESKTOP_EPOCH.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "system" fn on_foreground(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if !NATIVE_TRAY.load(Ordering::SeqCst) || !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    if hwnd.0.is_null() || is_ours(hwnd) {
        return;
    }
    if class_name(hwnd).is_some_and(|name| is_native_tray_class(&name)) {
        return;
    }
    NATIVE_TRAY.store(false, Ordering::SeqCst);
    set_trays_visible(false);
}

// ---------------------------------------------------------------------------
// Commands and autostart.

/// Temporarily shows the native Windows taskbar so its tray icons can be used.
/// It hides again when the foreground moves to something outside the tray.
#[tauri::command]
pub fn show_native_tray(_app: AppHandle) -> Result<(), String> {
    if !ACTIVE.load(Ordering::SeqCst) {
        return Ok(());
    }
    // The flag goes first, so the foreground watcher cannot hide the trays again
    // before this call has shown them.
    NATIVE_TRAY.store(true, Ordering::SeqCst);
    set_trays_visible(true);
    let tray = tray_windows()
        .into_iter()
        .find(|hwnd| class_name(*hwnd).as_deref() == Some("Shell_TrayWnd"));
    if let Some(tray) = tray {
        // SAFETY: tray is a live window handle. SetForegroundWindow may refuse, and
        // that is harmless.
        let _ = unsafe { SetForegroundWindow(tray) };
    }
    Ok(())
}

/// Reads `HKCU\...\Run\Lucent`. `Ok(None)` when the value is absent.
fn read_run_value() -> Result<Option<String>, String> {
    let key = open_run_key(KEY_QUERY_VALUE)?;
    let name = w!("Lucent");
    let mut kind = REG_VALUE_TYPE(0);
    let mut size = 0u32;
    // SAFETY: a null data pointer asks only for the size. kind and size are valid out-pointers.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name,
            None,
            Some(&mut kind as *mut REG_VALUE_TYPE),
            None,
            Some(&mut size as *mut u32),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    status.ok().map_err(|e| format!("cannot read the Run value: {e}"))?;
    let mut buf = vec![0u8; size as usize];
    let mut len = size;
    // SAFETY: buf has `len` writable bytes, and len is passed in as its size.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name,
            None,
            Some(&mut kind as *mut REG_VALUE_TYPE),
            Some(buf.as_mut_ptr()),
            Some(&mut len as *mut u32),
        )
    };
    status.ok().map_err(|e| format!("cannot read the Run value: {e}"))?;
    let used = (len as usize).min(buf.len());
    let units: Vec<u16> = buf[..used]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Ok(Some(String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()))
}

/// Writes or deletes `HKCU\...\Run\Lucent`.
fn write_run_value(enabled: bool) -> Result<(), String> {
    let name = w!("Lucent");
    if enabled {
        let key = open_run_key(KEY_SET_VALUE)?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let command = run_command(&exe.to_string_lossy());
        let bytes: Vec<u8> = command
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        // SAFETY: key is an open handle with KEY_SET_VALUE, and bytes is a live
        // UTF-16 buffer with its exact length.
        let status = unsafe { RegSetValueExW(key.0, name, None, REG_SZ, Some(&bytes)) };
        status.ok().map_err(|e| format!("cannot write the Run value: {e}"))
    } else {
        let key = open_run_key(KEY_SET_VALUE)?;
        // SAFETY: key is an open handle with KEY_SET_VALUE and the name is static.
        let status = unsafe { RegDeleteValueW(key.0, name) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        status.ok().map_err(|e| format!("cannot remove the Run value: {e}"))
    }
}

fn open_run_key(access: REG_SAM_FLAGS) -> Result<RegKey, String> {
    let mut key = HKEY(std::ptr::null_mut());
    // SAFETY: the subkey is a static literal, and key is a valid out-pointer that
    // receives an open handle on success.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            None,
            access,
            &mut key,
        )
    };
    status
        .ok()
        .map_err(|e| format!("cannot open the Run key: {e}"))?;
    Ok(RegKey(key))
}

/// Closes a registry key when dropped.
struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by open_run_key and is closed only here.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// True when the Run value exists and points at the current exe.
pub fn autostart_enabled() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    matches!(
        read_run_value(),
        Ok(Some(stored)) if run_value_matches(&stored, &exe.to_string_lossy())
    )
}

pub fn set_autostart(enabled: bool) -> Result<(), String> {
    write_run_value(enabled)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const LWIN: u32 = VK_LWIN;
    const RWIN: u32 = VK_RWIN;
    const E: u32 = 0x45;

    fn screen(x: i32, y: i32, width: u32, height: u32, scale: f64) -> Screen {
        Screen {
            x,
            y,
            width,
            height,
            scale,
        }
    }

    /// Runs a sequence of (vk, down, injected) events and returns the actions.
    fn run(events: &[(u32, bool, bool)]) -> (WinKeys, Vec<Action>) {
        let mut state = WinKeys::default();
        let mut actions = Vec::new();
        for &(vk, down, injected) in events {
            let (next, action) = on_key(state, vk, down, injected);
            state = next;
            actions.push(action);
        }
        (state, actions)
    }

    #[test]
    fn lone_lwin_release_masks_and_starts() {
        let (state, actions) = run(&[(LWIN, true, false), (LWIN, false, false)]);
        assert_eq!(actions, vec![Action::Pass, Action::MaskThenStart]);
        assert_eq!(state, WinKeys::default());
    }

    #[test]
    fn lone_rwin_release_masks_and_starts() {
        let (state, actions) = run(&[(RWIN, true, false), (RWIN, false, false)]);
        assert_eq!(actions, vec![Action::Pass, Action::MaskThenStart]);
        assert_eq!(state, WinKeys::default());
    }

    #[test]
    fn win_e_combo_passes_and_does_not_start() {
        let (state, actions) = run(&[
            (LWIN, true, false),
            (E, true, false),
            (E, false, false),
            (LWIN, false, false),
        ]);
        assert_eq!(
            actions,
            vec![Action::Pass, Action::Pass, Action::Pass, Action::Pass]
        );
        assert_eq!(state, WinKeys::default());
    }

    #[test]
    fn win_held_injected_keys_are_ignored() {
        // Our own Win+N (SendInput) while Win is held must not make it a combo.
        let (_, actions) = run(&[
            (LWIN, true, false),
            (E, true, true),
            (E, false, true),
            (LWIN, false, false),
        ]);
        assert_eq!(actions.last(), Some(&Action::MaskThenStart));
    }

    #[test]
    fn injected_win_events_are_ignored() {
        // A synthetic Win+N from send_shortcut: no start, no state change.
        let (state, actions) = run(&[
            (LWIN, true, true),
            (E, true, true),
            (E, false, true),
            (LWIN, false, true),
        ]);
        assert!(actions.iter().all(|a| *a == Action::Pass));
        assert_eq!(state, WinKeys::default());
    }

    #[test]
    fn win_autorepeat_is_not_a_combo() {
        let (_, actions) = run(&[
            (LWIN, true, false),
            (LWIN, true, false),
            (LWIN, true, false),
            (LWIN, false, false),
        ]);
        assert_eq!(actions.last(), Some(&Action::MaskThenStart));
    }

    #[test]
    fn both_win_keys_held_is_a_combo() {
        let (state, actions) = run(&[
            (LWIN, true, false),
            (RWIN, true, false),
            (LWIN, false, false),
            (RWIN, false, false),
        ]);
        assert!(actions.iter().all(|a| *a == Action::Pass));
        assert_eq!(state, WinKeys::default());
    }

    #[test]
    fn win_up_without_down_passes() {
        let (state, actions) = run(&[(LWIN, false, false)]);
        assert_eq!(actions, vec![Action::Pass]);
        assert_eq!(state, WinKeys::default());
    }

    #[test]
    fn other_keys_without_win_pass_through() {
        let (state, actions) = run(&[(E, true, false), (E, false, false)]);
        assert!(actions.iter().all(|a| *a == Action::Pass));
        assert_eq!(state, WinKeys::default());
    }

    #[test]
    fn win_released_before_other_key_is_lone() {
        let (_, actions) = run(&[
            (LWIN, true, false),
            (LWIN, false, false),
            (E, true, false),
        ]);
        assert_eq!(actions[1], Action::MaskThenStart);
        assert_eq!(actions[2], Action::Pass);
    }

    #[test]
    fn combo_key_autorepeat_stays_a_combo() {
        let (_, actions) = run(&[
            (LWIN, true, false),
            (E, true, false),
            (E, true, false),
            (E, true, false),
            (LWIN, false, false),
        ]);
        assert_eq!(actions.last(), Some(&Action::Pass));
    }

    #[test]
    fn dock_rect_at_100_percent_is_bottom_aligned() {
        let rect = dock_rect(screen(0, 0, 1920, 1080, 1.0));
        assert_eq!(
            rect,
            DockRect {
                x: 0,
                y: 976,
                width: 1920,
                height: 104
            }
        );
    }

    #[test]
    fn dock_rect_at_150_percent_scales_the_height() {
        let rect = dock_rect(screen(0, 0, 2880, 1620, 1.5));
        assert_eq!(
            rect,
            DockRect {
                x: 0,
                y: 1620 - 156,
                width: 2880,
                height: 156
            }
        );
    }

    #[test]
    fn dock_rect_on_an_offset_monitor() {
        // A monitor left of and above the primary origin.
        let rect = dock_rect(screen(-1920, -300, 1920, 1080, 1.0));
        assert_eq!(rect.x, -1920);
        assert_eq!(rect.y, -300 + 1080 - 104);
        assert_eq!(rect.width, 1920);
        assert_eq!(rect.height, 104);
    }

    #[test]
    fn reserve_rect_at_150_percent_covers_the_bottom_96_px() {
        let rect = reserve_rect(screen(0, 0, 2880, 1620, 1.5));
        assert_eq!(rect.left, 0);
        assert_eq!(rect.top, 1620 - 96);
        assert_eq!(rect.right, 2880);
        assert_eq!(rect.bottom, 1620);
    }

    #[test]
    fn reserve_rect_on_an_offset_monitor() {
        let rect = reserve_rect(screen(1920, 0, 1920, 1080, 1.0));
        assert_eq!(rect.left, 1920);
        assert_eq!(rect.right, 3840);
        assert_eq!(rect.top, 1080 - 64);
        assert_eq!(rect.bottom, 1080);
    }

    #[test]
    fn saved_state_round_trips() {
        let state = SavedState::from_raw(1);
        let text = encode_state(&state).unwrap();
        assert_eq!(parse_state(&text).unwrap(), state);
        assert_eq!(state.state_value(), 1);
    }

    #[test]
    fn saved_state_from_raw_reads_the_autohide_bit() {
        assert!(SavedState::from_raw(1).autohide);
        assert!(SavedState::from_raw(3).autohide);
        assert!(!SavedState::from_raw(2).autohide);
        assert!(!SavedState::from_raw(0).autohide);
    }

    #[test]
    fn saved_state_without_raw_falls_back_to_autohide() {
        let on = parse_state(r#"{"autohide": true}"#).unwrap();
        let off = parse_state(r#"{"autohide": false}"#).unwrap();
        assert_eq!(on.state_value(), ABS_AUTOHIDE);
        assert_eq!(off.state_value(), 0);
    }

    #[test]
    fn corrupt_saved_state_is_an_error() {
        assert!(parse_state("").is_err());
        assert!(parse_state("{not json").is_err());
        assert!(parse_state(r#"{"autohide": "yes"}"#).is_err());
    }

    #[test]
    fn run_value_matches_the_quoted_exe_path() {
        let exe = r"C:\Users\alex\AppData\Local\Programs\Lucent\lucent.exe";
        assert!(run_value_matches(&run_command(exe), exe));
    }

    #[test]
    fn run_value_comparison_ignores_case() {
        assert!(run_value_matches(
            r#""c:\users\ALEX\appdata\local\programs\lucent\LUCENT.EXE""#,
            r"C:\Users\alex\AppData\Local\Programs\Lucent\lucent.exe"
        ));
    }

    #[test]
    fn run_value_for_another_exe_does_not_match() {
        assert!(!run_value_matches(
            r#""C:\Other\lucent.exe""#,
            r"C:\Users\alex\AppData\Local\Programs\Lucent\lucent.exe"
        ));
        assert!(!run_value_matches("", r"C:\lucent.exe"));
    }

    #[test]
    fn run_value_accepts_an_unquoted_value_and_stray_whitespace() {
        assert!(run_value_matches(r"  C:\dev\lucent.exe  ", r"C:\dev\lucent.exe"));
    }

    #[test]
    fn shell_tray_class_predicate_matches_only_the_taskbars() {
        assert!(is_shell_tray_class("Shell_TrayWnd"));
        assert!(is_new_taskbar(0x10, 0x20));
        assert!(is_new_taskbar(0, 0x20));
        assert!(!is_new_taskbar(0x10, 0x10));
        assert!(!is_new_taskbar(0x10, 0));
        assert!(is_shell_tray_class("Shell_SecondaryTrayWnd"));
        assert!(!is_shell_tray_class("Progman"));
        assert!(!is_shell_tray_class("Xaml_WindowedPopupClass"));
        assert!(!is_shell_tray_class("shell_traywnd"));
    }

    #[test]
    fn native_tray_family_includes_the_overflow_flyout() {
        assert!(is_native_tray_class("Shell_TrayWnd"));
        assert!(is_native_tray_class("NotifyIconOverflowWindow"));
        assert!(is_native_tray_class("TopLevelWindowForOverflowXamlIsland"));
        assert!(is_native_tray_class("Xaml_WindowedPopupClass"));
        assert!(!is_native_tray_class("CabinetWClass"));
        assert!(!is_native_tray_class("Windows.UI.Core.CoreWindow"));
    }

    #[test]
    fn keys_for_epoch_keeps_the_state_of_the_current_desktop() {
        let state = WinKeys {
            lwin: true,
            rwin: false,
            combo: true,
        };
        assert_eq!(keys_for_epoch((4, state), 4), state);
    }

    #[test]
    fn desktop_switch_drops_the_win_state() {
        // Win held, L pressed (a combo), then a desktop switch, then a lone Win tap.
        let (held, _) = on_key(WinKeys::default(), LWIN, true, false);
        let (combo, _) = on_key(held, 0x4C, true, false);
        let (after_l, _) = on_key(combo, 0x4C, false, false);
        let stored = (1u32, after_l);
        // The switch moved the epoch on, so the stale state is gone.
        let fresh = keys_for_epoch(stored, 2);
        let (fresh, _) = on_key(fresh, LWIN, true, false);
        let (_, action) = on_key(fresh, LWIN, false, false);
        assert_eq!(action, Action::MaskThenStart);
    }

    unsafe extern "system" fn unresponsive_test_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: the arguments are the ones Windows passed to this window procedure.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    #[test]
    fn tray_hide_does_not_wait_for_an_unresponsive_owner() {
        use std::time::{Duration, Instant};
        use windows::Win32::UI::WindowsAndMessaging::{
            DestroyWindow, WS_EX_TOOLWINDOW, WS_VISIBLE,
        };
        let (tx, rx) = std::sync::mpsc::channel::<isize>();
        let owner = std::thread::spawn(move || {
            let instance = module_instance().unwrap();
            let class = w!("LucentTestUnresponsiveTray");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(unresponsive_test_proc),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            };
            // SAFETY: wc and the class name are valid for the call.
            unsafe { RegisterClassW(&wc) };
            // SAFETY: the class was registered above. The window is a visible,
            // off-screen 1x1 tool window owned by this test thread.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    class,
                    w!(""),
                    WS_POPUP | WS_VISIBLE,
                    -32000,
                    -32000,
                    1,
                    1,
                    None,
                    None,
                    Some(instance),
                    None,
                )
            }
            .unwrap();
            tx.send(hwnd_bits(hwnd)).unwrap();
            // This thread does not pump messages while the test measures.
            std::thread::sleep(Duration::from_millis(1500));
            // SAFETY: hwnd is the window created above, destroyed on its own thread.
            let _ = unsafe { DestroyWindow(hwnd) };
        });
        let hwnd = hwnd_from_bits(rx.recv().unwrap());
        let started = Instant::now();
        show_async(&[hwnd], SW_HIDE);
        let elapsed = started.elapsed();
        owner.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(100),
            "hiding took {elapsed:?} with an unresponsive owner"
        );
    }
}

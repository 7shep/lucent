//! Fullscreen awareness: move or hide the island while an app is fullscreen.
//!
//! The pill prefers the primary monitor. If the primary has a fullscreen
//! window, it moves to the first other monitor without one, and is hidden when
//! none is free. Detection is event-driven: a dedicated thread runs a message
//! loop with WinEvent hooks (foreground, minimise, location, destroy, hide) and
//! a hidden window that receives WM_DISPLAYCHANGE. Those events start a one-shot
//! debounce timer. When it fires, the decision is re-run on the main thread and
//! the window is only touched when the target changes. Nothing polls.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::size_of;

use tauri::{AppHandle, Manager};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClassNameW, GetForegroundWindow,
    GetMessageW, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    IsZoomed, KillTimer, RegisterClassW, SetTimer, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, GWL_STYLE, MSG, OBJID_WINDOW, WINDOW_EX_STYLE,
    WINEVENT_OUTOFCONTEXT, WM_DISPLAYCHANGE, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_POPUP,
};

use crate::island::{self, Screen};

/// Coalescing window for bursts of events.
const DEBOUNCE_MS: u32 = 150;
const TIMER_ID: usize = 1;

/// Shell windows that cover the monitor but are never "a fullscreen app".
const SHELL_CLASSES: [&str; 4] = ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"];

/// (eventmin, eventmax) ranges to hook, all out of context.
const HOOKED_EVENTS: [(u32, u32); 5] = [
    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
    (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
    (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
    (EVENT_OBJECT_DESTROY, EVENT_OBJECT_DESTROY),
    (EVENT_OBJECT_HIDE, EVENT_OBJECT_HIDE),
];

/// Where the pill should be. `Show(i)` is an index into the screens slice.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Show(usize),
    Hidden,
}

/// Pure decision. `monitors` must be in a stable order, `primary_index` names
/// the primary monitor, and `fullscreen_on[i]` says monitor `i` has a fullscreen
/// window. Missing flags count as "not fullscreen".
pub fn choose_target(monitors: &[Screen], primary_index: usize, fullscreen_on: &[bool]) -> Target {
    let busy = |i: usize| fullscreen_on.get(i).copied().unwrap_or(false);
    if primary_index < monitors.len() && !busy(primary_index) {
        return Target::Show(primary_index);
    }
    (0..monitors.len())
        .find(|&i| i != primary_index && !busy(i))
        .map_or(Target::Hidden, Target::Show)
}

/// True when `window` covers all of `monitor`.
pub fn covers(window: RECT, monitor: RECT) -> bool {
    window.left <= monitor.left
        && window.top <= monitor.top
        && window.right >= monitor.right
        && window.bottom >= monitor.bottom
}

/// Whether a foreground window counts as fullscreen, given its geometry and style.
/// A maximized window with a caption also covers the monitor when the taskbar
/// auto-hides, so it does not count. Borderless and captioned-but-not-maximized
/// covering windows do. Exclusive D3D fullscreen is decided separately.
pub fn looks_fullscreen(covers: bool, zoomed: bool, has_caption: bool) -> bool {
    covers && !(zoomed && has_caption)
}

/// True when the monitor rect matches the screen's position and size.
fn same_rect(rect: RECT, screen: &Screen) -> bool {
    rect.left == screen.x
        && rect.top == screen.y
        && rect.right - rect.left == screen.width as i32
        && rect.bottom - rect.top == screen.height as i32
}

/// Starts the watcher thread. Returns immediately.
pub fn start(app: AppHandle) {
    let _ = std::thread::Builder::new()
        .name("lucent-fullscreen".into())
        .spawn(move || watch_thread(app));
}

thread_local! {
    /// State used by the callbacks. Only the watcher thread touches it.
    static WATCH: RefCell<Option<Watch>> = const { RefCell::new(None) };
}

struct Watch {
    app: AppHandle,
    window: HWND,
    /// A debounce timer is armed.
    pending: bool,
}

fn watch_thread(app: AppHandle) {
    let Some(window) = create_watch_window() else {
        eprintln!("lucent: fullscreen watcher window failed");
        return;
    };
    WATCH.with(|w| {
        *w.borrow_mut() = Some(Watch {
            app,
            window,
            pending: false,
        })
    });

    let hooks: Vec<HWINEVENTHOOK> = HOOKED_EVENTS
        .iter()
        .map(|&(min, max)| unsafe {
            SetWinEventHook(min, max, None, Some(on_win_event), 0, 0, WINEVENT_OUTOFCONTEXT)
        })
        .filter(|hook| !hook.0.is_null())
        .collect();

    // Evaluate once at startup, then only on events.
    schedule();

    // Hook callbacks and the watch window are dispatched from this loop.
    let mut msg = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if result <= 0 {
            break;
        }
        unsafe {
            DispatchMessageW(&msg);
        }
    }

    for hook in hooks {
        unsafe {
            let _ = UnhookWinEvent(hook);
        }
    }
}

/// Hidden top-level window. Message-only windows miss WM_DISPLAYCHANGE, so this
/// one is a popup that is never shown.
fn create_watch_window() -> Option<HWND> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(PCWSTR::null()).ok()?.into();
        let class = w!("LucentFullscreenWatch");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(watch_proc),
            hInstance: instance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
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
        .ok()
    }
}

unsafe extern "system" fn watch_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE => schedule(),
        WM_TIMER if wparam.0 == TIMER_ID => fire(hwnd),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

/// WinEvent callback. Keeps only window-level events, and location changes for
/// the foreground window.
unsafe extern "system" fn on_win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 {
        return;
    }
    if event == EVENT_OBJECT_LOCATIONCHANGE && hwnd != unsafe { GetForegroundWindow() } {
        return;
    }
    schedule();
}

/// Arms the one-shot debounce timer unless one is already armed. Events that
/// arrive while it is armed are absorbed, so re-evaluation runs at most once
/// per DEBOUNCE_MS.
fn schedule() {
    WATCH.with(|w| {
        let mut guard = w.borrow_mut();
        let Some(slot) = guard.as_mut() else {
            return;
        };
        if slot.pending {
            return;
        }
        let armed = unsafe { SetTimer(Some(slot.window), TIMER_ID, DEBOUNCE_MS, None) };
        slot.pending = armed != 0;
    });
}

/// Debounce timer fired: disarm it and re-run the decision on the main thread.
fn fire(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER_ID);
    }
    let app = WATCH.with(|w| {
        let mut guard = w.borrow_mut();
        let slot = guard.as_mut()?;
        slot.pending = false;
        Some(slot.app.clone())
    });
    if let Some(app) = app {
        let main = app.clone();
        let _ = app.run_on_main_thread(move || apply(&main));
    }
}

/// Runs on the main thread. Gathers monitors and fullscreen state, decides, and
/// applies the result to the island.
fn apply(app: &AppHandle) {
    let Some(window) = app.get_webview_window(island::LABEL) else {
        return;
    };
    let Ok(monitors) = window.available_monitors() else {
        return;
    };
    if monitors.is_empty() {
        // Nothing to place on. Keep the current state rather than hiding.
        return;
    }
    let mut screens: Vec<Screen> = monitors.iter().map(island::screen_of).collect();
    screens.sort_by_key(|s| (s.x, s.y));

    let primary = window
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| island::screen_of(&m));
    let primary_index = primary
        .and_then(|p| screens.iter().position(|s| *s == p))
        .unwrap_or(0);

    let fullscreen = unsafe { foreground_fullscreen_monitor() };
    let fullscreen_on: Vec<bool> = screens
        .iter()
        .map(|s| fullscreen.is_some_and(|rect| same_rect(rect, s)))
        .collect();

    let target = match choose_target(&screens, primary_index, &fullscreen_on) {
        Target::Show(i) => Some(screens[i]),
        Target::Hidden => None,
    };
    if let Err(err) = island::set_target(app, target) {
        eprintln!("lucent: fullscreen placement failed: {err}");
    }
}

/// Monitor rect of the foreground window when it is fullscreen. Only the
/// foreground window is checked, which is how Windows decides too.
///
/// Fullscreen means the window rect covers its monitor. An exclusive D3D
/// fullscreen app also counts, through SHQueryUserNotificationState, because
/// its window rect may not match the monitor.
unsafe fn foreground_fullscreen_monitor() -> Option<RECT> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() || !is_candidate(hwnd) {
        return None;
    }
    let mut window = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut window) }.ok()?;
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) };
    if monitor.0.is_null() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    let zoomed = unsafe { IsZoomed(hwnd) }.as_bool();
    // Style bits fit in the low 32 bits.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    let has_caption = style & WS_CAPTION.0 == WS_CAPTION.0;
    let windowed_fullscreen = looks_fullscreen(
        covers(window, info.rcMonitor),
        zoomed,
        has_caption,
    );
    let exclusive =
        unsafe { SHQueryUserNotificationState() }.ok() == Some(QUNS_RUNNING_D3D_FULL_SCREEN);
    (windowed_fullscreen || exclusive).then_some(info.rcMonitor)
}

/// Visible, not minimised, not cloaked, not ours, and not a shell window.
unsafe fn is_candidate(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return false;
        }
        let mut cloaked: u32 = 0;
        let cloak = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        );
        if cloak.is_ok() && cloaked != 0 {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
        if pid == std::process::id() {
            return false;
        }
        let mut class = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut class);
        if len > 0 {
            let name = String::from_utf16_lossy(&class[..len as usize]);
            if SHELL_CLASSES.contains(&name.as_str()) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(x: i32) -> Screen {
        Screen {
            x,
            y: 0,
            width: 1920,
            height: 1080,
            scale: 1.0,
        }
    }

    fn one() -> Vec<Screen> {
        vec![screen(0)]
    }

    fn two() -> Vec<Screen> {
        vec![screen(0), screen(1920)]
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn single_monitor_free_shows_primary() {
        assert_eq!(choose_target(&one(), 0, &[false]), Target::Show(0));
    }

    #[test]
    fn single_monitor_fullscreen_hides() {
        assert_eq!(choose_target(&one(), 0, &[true]), Target::Hidden);
    }

    #[test]
    fn primary_fullscreen_moves_to_secondary() {
        assert_eq!(choose_target(&two(), 0, &[true, false]), Target::Show(1));
    }

    #[test]
    fn both_fullscreen_hides() {
        assert_eq!(choose_target(&two(), 0, &[true, true]), Target::Hidden);
    }

    #[test]
    fn only_secondary_fullscreen_stays_on_primary() {
        assert_eq!(choose_target(&two(), 0, &[false, true]), Target::Show(0));
    }

    #[test]
    fn primary_not_at_index_zero() {
        // Primary is the second entry (index 1).
        assert_eq!(choose_target(&two(), 1, &[true, false]), Target::Show(1));
        assert_eq!(choose_target(&two(), 1, &[false, false]), Target::Show(1));
        // Primary busy: the pill goes to the free monitor at index 0.
        assert_eq!(choose_target(&two(), 1, &[false, true]), Target::Show(0));
        assert_eq!(choose_target(&two(), 1, &[true, true]), Target::Hidden);
    }

    #[test]
    fn primary_fullscreen_skips_busy_monitors_in_order() {
        let three = vec![screen(0), screen(1920), screen(3840)];
        // Primary (index 1) busy, index 0 busy: index 2 is the first free one.
        assert_eq!(
            choose_target(&three, 1, &[true, true, false]),
            Target::Show(2)
        );
        // Primary busy, index 0 free: the first free monitor in order wins.
        assert_eq!(
            choose_target(&three, 1, &[false, true, true]),
            Target::Show(0)
        );
        assert_eq!(
            choose_target(&three, 1, &[true, true, true]),
            Target::Hidden
        );
    }

    #[test]
    fn missing_flags_count_as_free() {
        assert_eq!(choose_target(&two(), 0, &[]), Target::Show(0));
    }

    #[test]
    fn no_monitors_hides() {
        assert_eq!(choose_target(&[], 0, &[]), Target::Hidden);
    }

    #[test]
    fn covers_requires_the_full_monitor() {
        let mon = rect(0, 0, 1920, 1080);
        assert!(covers(rect(0, 0, 1920, 1080), mon));
        assert!(covers(rect(-8, -8, 1928, 1088), mon));
        assert!(!covers(rect(0, 0, 1920, 1040), mon));
        assert!(!covers(rect(100, 100, 1820, 980), mon));
    }

    #[test]
    fn borderless_covering_window_is_fullscreen() {
        assert!(looks_fullscreen(true, false, false));
    }

    #[test]
    fn maximized_captioned_window_is_not_fullscreen() {
        // Taskbar auto-hide makes a maximized browser cover the whole monitor.
        assert!(!looks_fullscreen(true, true, true));
    }

    #[test]
    fn captioned_covering_window_that_is_not_maximized_is_fullscreen() {
        assert!(looks_fullscreen(true, false, true));
    }

    #[test]
    fn maximized_borderless_covering_window_is_fullscreen() {
        assert!(looks_fullscreen(true, true, false));
    }

    #[test]
    fn non_covering_window_is_not_fullscreen() {
        assert!(!looks_fullscreen(false, false, false));
        assert!(!looks_fullscreen(false, true, true));
        assert!(!looks_fullscreen(false, false, true));
    }

    #[test]
    fn covers_is_false_for_a_window_on_another_monitor() {
        assert!(!covers(rect(1920, 0, 3840, 1080), rect(0, 0, 1920, 1080)));
    }
}

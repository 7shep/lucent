//! Running windows and the merged dock model (pins + open windows), kept up to
//! date by window events. See docs/taskbar-spec.md, module `tasks`.
//!
//! A dedicated thread runs a message loop with out-of-context WinEvent hooks and
//! a hidden window registered with RegisterShellHookWindow. Events arm a one-shot
//! debounce timer. When it fires, the dock list is rebuilt on that same thread,
//! and `dock-changed` is emitted to the dock window only if the list changed.
//! Nothing polls.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use tauri::{AppHandle, Emitter};
use windows::core::{w, Interface, BOOL, IUnknown, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, HWND, LPARAM, LRESULT,
    PROPERTYKEY, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IBindCtx, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VK_MENU,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::{
    IShellItem, IShellLinkW, SHCreateItemFromParsingName, ShellLink, SIGDN_NORMALDISPLAY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, EnumWindows, GetClassNameW,
    GetForegroundWindow, GetMessageW, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, KillTimer,
    PostMessageW, RegisterClassW, RegisterShellHookWindow, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, ShowWindowAsync, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY,
    EVENT_OBJECT_HIDE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, GWL_EXSTYLE, GW_OWNER, MSG, OBJID_WINDOW,
    SW_MINIMIZE, SW_RESTORE, WINDOW_EX_STYLE, WINEVENT_OUTOFCONTEXT, WM_CLOSE, WM_TIMER, WNDCLASSW,
    WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::model::{App, DockItem, DockWindow};

/// Event emitted to the dock window with the full `Vec<DockItem>` whenever it changes.
pub const DOCK_CHANGED: &str = "dock-changed";

/// Label of the dock window (see tauri.conf.json).
const DOCK_LABEL: &str = "dock";
/// Coalescing window for bursts of events.
const DEBOUNCE_MS: u32 = 100;
/// Coalescing window when the only events are title changes (NAMECHANGE storms).
const NAMECHANGE_DEBOUNCE_MS: u32 = 500;
const TIMER_ID: usize = 1;
/// HSHELL_REDRAW | HSHELL_HIGHBIT: the shell hook code for a window flashing.
const HSHELL_FLASH: usize = 0x8006;

/// Windows that are never taskbar entries.
const SHELL_CLASSES: [&str; 4] = ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"];
/// File Explorer's window class; its windows all belong to this AUMID.
const EXPLORER_CLASS: &str = "CabinetWClass";
const EXPLORER_AUMID: &str = "Microsoft.Windows.Explorer";
const APPS_FOLDER_PREFIX: &str = "shell:AppsFolder\\";

/// PKEY_AppUserModel_ID (propsys.h). Declared here so tasks.rs does not need the
/// Win32_Storage_EnhancedStorage feature.
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

/// (eventmin, eventmax) ranges to hook, all out of context.
const HOOKED_EVENTS: [(u32, u32); 5] = [
    // create, destroy, show, hide
    (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
    (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
    (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
    (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
];

// ---------------------------------------------------------------------------
// Pure model: eligibility, matching, merging, decisions. Unit-tested.
// ---------------------------------------------------------------------------

/// Facts about one top-level window, gathered from Win32.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowFacts {
    pub visible: bool,
    pub cloaked: bool,
    pub title: String,
    pub has_owner: bool,
    pub ex_style: u32,
    pub class: String,
    /// Owned by this Lucent process (the pill, the dock, the watcher window).
    pub same_process: bool,
}

/// True when the window belongs on the taskbar.
pub fn is_taskbar_window(f: &WindowFacts) -> bool {
    if !f.visible || f.cloaked || f.title.is_empty() || f.same_process {
        return false;
    }
    if SHELL_CLASSES.contains(&f.class.as_str()) {
        return false;
    }
    let app_window = f.ex_style & WS_EX_APPWINDOW.0 != 0;
    let tool_window = f.ex_style & WS_EX_TOOLWINDOW.0 != 0;
    let no_activate = f.ex_style & WS_EX_NOACTIVATE.0 != 0;
    let shape_ok = app_window || (!f.has_owner && !tool_window);
    let focus_ok = app_window || !no_activate;
    shape_ok && focus_ok
}

/// A taskbar pin, reduced to what matching needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinEntry {
    pub id: String,
    pub name: String,
    pub launch: String,
    /// Exe the pin starts: the .lnk target, or the launch path itself for an exe.
    pub target: Option<String>,
    /// .lnk arguments ("" when none).
    pub args: String,
}

/// An open window, reduced to what matching and merging need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowEntry {
    pub hwnd: i64,
    pub title: String,
    pub aumid: Option<String>,
    /// Full path of the window's process image.
    pub exe: Option<String>,
    /// The process has a package identity, so it launches through AppsFolder.
    pub packaged: bool,
    /// Display name for an unpinned group (AppsFolder name, FileDescription, or title).
    pub name: String,
    pub foreground: bool,
    pub flashing: bool,
}

/// Case-insensitive equality for paths, AUMIDs and file names.
fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Last path component.
pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// File name without its extension.
pub fn file_stem(path: &str) -> &str {
    let name = file_name(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// True when one of the whitespace- or `=`-separated arguments names `file`
/// (quotes ignored). Discord's pin `Update.exe --processStart Discord.exe`
/// names `Discord.exe`.
fn args_mention(args: &str, file: &str) -> bool {
    !file.is_empty()
        && args
            .split(|c: char| c.is_whitespace() || c == '=')
            .map(|token| token.trim_matches('"'))
            .any(|token| eq_ci(token, file))
}

/// True when the window belongs to the pin. Any one of the rules is enough:
/// window AUMID == pin id; window exe path == pin target path; the pin's .lnk
/// arguments name the window's exe file; the window's exe file stem equals the
/// pin name or the pin target's file stem.
pub fn pin_matches(pin: &PinEntry, win: &WindowEntry) -> bool {
    if let Some(aumid) = &win.aumid {
        if eq_ci(aumid, &pin.id) {
            return true;
        }
    }
    let Some(exe) = win.exe.as_deref() else {
        return false;
    };
    if let Some(target) = &pin.target {
        if eq_ci(exe, target) {
            return true;
        }
    }
    if args_mention(&pin.args, file_name(exe)) {
        return true;
    }
    let stem = file_stem(exe);
    if stem.is_empty() {
        return false;
    }
    eq_ci(stem, &pin.name) || pin.target.as_deref().is_some_and(|t| eq_ci(stem, file_stem(t)))
}

/// Grouping key of a window: its AUMID, else its lowercase exe path, else the hwnd.
pub fn identity_key(aumid: Option<&str>, exe: Option<&str>, hwnd: i64) -> String {
    if let Some(aumid) = aumid.filter(|a| !a.is_empty()) {
        return aumid.to_lowercase();
    }
    if let Some(exe) = exe.filter(|e| !e.is_empty()) {
        return exe.to_lowercase();
    }
    format!("hwnd:{hwnd}")
}

/// Launch string for an unpinned group: packaged apps go through AppsFolder by
/// AUMID; desktop apps launch by exe path.
pub fn unpinned_launch(aumid: Option<&str>, packaged: bool, exe: Option<&str>) -> String {
    let in_windows_apps = exe.is_some_and(|e| e.to_ascii_lowercase().contains("\\windowsapps\\"));
    match (aumid, exe) {
        (Some(a), _) if packaged || in_windows_apps => format!("{APPS_FOLDER_PREFIX}{a}"),
        (_, Some(e)) if !e.is_empty() => e.to_string(),
        (Some(a), _) => format!("{APPS_FOLDER_PREFIX}{a}"),
        _ => String::new(),
    }
}

fn identity_of(win: &WindowEntry) -> String {
    identity_key(win.aumid.as_deref(), win.exe.as_deref(), win.hwnd)
}

fn make_item(id: String, name: String, launch: String, pinned: bool, members: &[&WindowEntry]) -> DockItem {
    DockItem {
        id,
        name,
        launch,
        pinned,
        windows: members
            .iter()
            .map(|w| DockWindow {
                hwnd: w.hwnd,
                title: w.title.clone(),
            })
            .collect(),
        active: members.iter().any(|w| w.foreground),
        flashing: members.iter().any(|w| w.flashing),
    }
}

/// Builds the dock list. Pins come first in pin order, each with the windows that
/// match it (a window goes to the first pin that matches it). Then unpinned
/// windows, grouped by identity in order of first appearance. `windows` is in
/// z-order, topmost first, and that order is kept inside each item.
pub fn merge(pins: &[PinEntry], windows: &[WindowEntry]) -> Vec<DockItem> {
    let mut claimed = vec![false; windows.len()];
    let mut items = Vec::with_capacity(pins.len());
    for pin in pins {
        let mut members: Vec<&WindowEntry> = Vec::new();
        for (i, win) in windows.iter().enumerate() {
            if !claimed[i] && pin_matches(pin, win) {
                claimed[i] = true;
                members.push(win);
            }
        }
        items.push(make_item(
            pin.id.clone(),
            pin.name.clone(),
            pin.launch.clone(),
            true,
            &members,
        ));
    }

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&WindowEntry>> = HashMap::new();
    for (i, win) in windows.iter().enumerate() {
        if claimed[i] {
            continue;
        }
        let key = identity_of(win);
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(win);
    }
    for key in order {
        let members = &groups[&key];
        let first = members[0];
        let name = if first.name.is_empty() {
            first.title.clone()
        } else {
            first.name.clone()
        };
        let launch = unpinned_launch(first.aumid.as_deref(), first.packaged, first.exe.as_deref());
        items.push(make_item(key, name, launch, false, members));
    }
    items
}

/// What the foreground decision says to do with a window the user clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// It is already the foreground window: minimise it.
    Minimize,
    /// Bring it to the front, restoring it first when minimised.
    Bring { restore: bool },
}

/// The "activate or minimise" decision for a clicked window.
pub fn activation_for(is_foreground: bool, is_minimized: bool) -> Activation {
    if is_foreground {
        Activation::Minimize
    } else {
        Activation::Bring {
            restore: is_minimized,
        }
    }
}

/// True when `hwnd` is one of the windows listed in the dock `items`.
pub fn is_dock_window(items: &[DockItem], hwnd: i64) -> bool {
    items
        .iter()
        .any(|item| item.windows.iter().any(|w| w.hwnd == hwnd))
}

/// The debounce delay for an event. NAMECHANGE storms (window titles churning)
/// get the longer delay, so they rebuild at most once per NAMECHANGE_DEBOUNCE_MS.
pub fn debounce_delay(namechange: bool) -> u32 {
    if namechange {
        NAMECHANGE_DEBOUNCE_MS
    } else {
        DEBOUNCE_MS
    }
}

/// True when a timer armed for `armed` ms should be re-armed for a new event
/// needing `delay` ms: only when that is sooner than what is already armed.
pub fn should_rearm(armed: Option<u32>, delay: u32) -> bool {
    armed.is_none_or(|current| delay < current)
}

/// A change to the set of flashing windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashEvent {
    Flash(i64),
    Foreground(i64),
    Destroyed(i64),
}

/// A window stops flashing when it becomes foreground or is destroyed.
pub fn apply_flash_event(flashing: &mut HashSet<i64>, event: FlashEvent) {
    match event {
        FlashEvent::Flash(hwnd) => {
            flashing.insert(hwnd);
        }
        FlashEvent::Foreground(hwnd) | FlashEvent::Destroyed(hwnd) => {
            flashing.remove(&hwnd);
        }
    }
}

/// True for the shell hook code HSHELL_FLASH (low word of wParam).
pub fn is_flash_message(wparam: usize) -> bool {
    wparam & 0xFFFF == HSHELL_FLASH
}

/// Stores `items` when they differ from the current list. Returns true when stored.
fn replace_if_changed(slot: &mut Option<Vec<DockItem>>, items: Vec<DockItem>) -> bool {
    if slot.as_deref() == Some(items.as_slice()) {
        return false;
    }
    *slot = Some(items);
    true
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    /// The latest dock list. None until the first build.
    items: Option<Vec<DockItem>>,
    /// Windows currently flashing for attention, as i64 HWNDs.
    flashing: HashSet<i64>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();
/// Resolved AppsFolder / FileDescription names, keyed by identity.
static NAMES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
/// Resolved .lnk targets, keyed by launch string.
static LNK: OnceLock<Mutex<HashMap<String, (Option<String>, String)>>> = OnceLock::new();

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn names() -> &'static Mutex<HashMap<String, String>> {
    NAMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lnk_cache() -> &'static Mutex<HashMap<String, (Option<String>, String)>> {
    LNK.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Stores the list and emits it to the dock window when it changed.
fn publish(items: Vec<DockItem>) {
    let payload = items.clone();
    {
        let mut st = locked(state());
        if !replace_if_changed(&mut st.items, items) {
            return;
        }
    }
    if let Some(app) = APP.get() {
        let _ = app.emit_to(DOCK_LABEL, DOCK_CHANGED, payload);
    }
}

fn note_flash(event: FlashEvent) {
    apply_flash_event(&mut locked(state()).flashing, event);
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

fn to_i64(hwnd: HWND) -> i64 {
    hwnd.0 as isize as i64
}

fn from_i64(value: i64) -> HWND {
    HWND(value as isize as *mut c_void)
}

fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Initialises COM (apartment-threaded) once per thread. The apartment is left
/// initialised: these threads live as long as the process.
fn ensure_com() {
    thread_local! {
        static COM_READY: Cell<bool> = const { Cell::new(false) };
    }
    COM_READY.with(|ready| {
        if !ready.get() {
            // SAFETY: no reserved pointer. S_FALSE and RPC_E_CHANGED_MODE both leave COM usable on this thread, so the result is ignored.
            let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            ready.set(true);
        }
    });
}

/// Takes ownership of a CoTaskMem string, copies it out and frees it.
fn take_pwstr(text: PWSTR) -> Option<String> {
    // SAFETY: text came from a COM allocator (GetDisplayName, PropVariantToStringAlloc) and is freed exactly once below.
    let owned = unsafe { text.to_string() }.ok();
    // SAFETY: the pointer is the CoTaskMem allocation returned to us; freed once.
    unsafe { CoTaskMemFree(Some(text.0 as *const c_void)) };
    owned.filter(|s| !s.is_empty())
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is closed exactly once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn foreground_window() -> HWND {
    // SAFETY: GetForegroundWindow takes no arguments and only reads window state.
    unsafe { GetForegroundWindow() }
}

/// Every top-level window, in z-order (topmost first).
fn top_level_windows() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the address of the Vec below, which outlives the EnumWindows call and is only touched from this thread.
        let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        list.push(hwnd);
        BOOL(1)
    }
    let mut list: Vec<HWND> = Vec::new();
    // SAFETY: the callback is valid for the whole call, and `list` outlives it.
    let _ = unsafe { EnumWindows(Some(collect), LPARAM(&mut list as *mut Vec<HWND> as isize)) };
    list
}

fn window_facts(hwnd: HWND) -> (WindowFacts, u32) {
    let mut pid = 0u32;
    // SAFETY: hwnd comes from EnumWindows; pid is a valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32)) };
    // SAFETY: read-only queries on a window handle from EnumWindows.
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    // SAFETY: read-only query; an error means no owner.
    let has_owner = matches!(unsafe { GetWindow(hwnd, GW_OWNER) }, Ok(owner) if !owner.0.is_null());
    // SAFETY: read-only query. Style bits fit in the low 32 bits.
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let facts = WindowFacts {
        visible,
        cloaked: is_cloaked(hwnd),
        title: window_title(hwnd),
        has_owner,
        ex_style,
        class: window_class(hwnd),
        same_process: pid == std::process::id(),
    };
    (facts, pid)
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: the out buffer is a u32, matching the size passed for DWMWA_CLOAKED.
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        )
    }
    .is_ok();
    ok && cloaked != 0
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: read-only query on a window handle.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: buf has room for the title and its terminator.
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
    let n = (copied.max(0) as usize).min(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: buf is a valid, writable UTF-16 buffer of the length passed.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    let n = (len.max(0) as usize).min(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// Per-process facts, computed once per snapshot.
struct ProcInfo {
    exe: Option<String>,
    packaged_aumid: Option<String>,
}

fn process_info(pid: u32) -> ProcInfo {
    // SAFETY: plain OpenProcess with query-only rights on a pid from GetWindowThreadProcessId.
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return ProcInfo {
            exe: None,
            packaged_aumid: None,
        };
    };
    let guard = OwnedHandle(handle);
    ProcInfo {
        exe: process_image(guard.0),
        packaged_aumid: process_aumid(guard.0),
    }
}

/// A process's display name: its exe's FileDescription, else the exe's file
/// name without extension. None when the process cannot be queried.
pub(crate) fn process_display_name(pid: u32) -> Option<String> {
    let exe = process_info(pid).exe?;
    file_description(&exe).or_else(|| {
        std::path::Path::new(&exe)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    })
}

fn process_image(handle: HANDLE) -> Option<String> {
    let mut buf = vec![0u16; 4096];
    let mut len = buf.len() as u32;
    // SAFETY: buf is valid for `len` UTF-16 units, and len is passed as its size.
    unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
    }
    .ok()?;
    let n = (len as usize).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n])).filter(|s| !s.is_empty())
}

/// AUMID of a packaged process; None for desktop processes.
fn process_aumid(handle: HANDLE) -> Option<String> {
    let mut len = 0u32;
    // SAFETY: a null buffer only asks for the required length.
    let first = unsafe { GetApplicationUserModelId(handle, &mut len, None) };
    if first != ERROR_INSUFFICIENT_BUFFER || len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: buf holds `len` UTF-16 units, and len is passed as its size.
    let status = unsafe {
        GetApplicationUserModelId(handle, &mut len, Some(PWSTR(buf.as_mut_ptr())))
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end])).filter(|s| !s.is_empty())
}

/// AppUserModelID set on the window itself.
fn property_aumid(hwnd: HWND) -> Option<String> {
    // SAFETY: hwnd is a window handle; the returned store is released on drop.
    let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(hwnd) }.ok()?;
    // SAFETY: store is valid, and the key is a const that lives for the call.
    let value = unsafe { store.GetValue(&PKEY_APP_USER_MODEL_ID) }.ok()?;
    // SAFETY: value is a valid PROPVARIANT that outlives the call.
    let text = unsafe { PropVariantToStringAlloc(&value) }.ok()?;
    take_pwstr(text)
}

/// Display name of an AUMID from the AppsFolder, e.g. "File Explorer".
fn apps_folder_name(aumid: &str) -> Option<String> {
    ensure_com();
    let wide = to_wide(&format!("{APPS_FOLDER_PREFIX}{aumid}"));
    // SAFETY: wide is NUL-terminated and lives across the call; no bind context.
    let item: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>) }.ok()?;
    // SAFETY: item is a valid IShellItem.
    let text = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }.ok()?;
    take_pwstr(text)
}

/// The exe's FileDescription from its version resource.
fn file_description(exe: &str) -> Option<String> {
    let wide = to_wide(exe);
    // SAFETY: wide is NUL-terminated and lives across the call.
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: data is `size` bytes, as passed.
    unsafe {
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr() as *mut c_void)
    }
    .ok()?;

    // Language and code page of the first translation entry.
    let mut ptr: *mut c_void = null_mut();
    let mut len = 0u32;
    let translation = w!("\\VarFileInfo\\Translation");
    // SAFETY: data is a version block from GetFileVersionInfoW; the out pointers are valid.
    let found = unsafe {
        VerQueryValueW(data.as_ptr() as *const c_void, translation, &mut ptr, &mut len)
    }
    .as_bool();
    if !found || len < 4 || ptr.is_null() {
        return None;
    }
    // SAFETY: a translation entry is two u16s and VerQueryValueW returned at least 4 bytes at ptr, inside data.
    let (lang, codepage) = unsafe {
        let pair = std::slice::from_raw_parts(ptr as *const u16, 2);
        (pair[0], pair[1])
    };

    let query = to_wide(&format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\FileDescription"));
    ptr = null_mut();
    len = 0;
    // SAFETY: same block as above; query is NUL-terminated.
    let found = unsafe {
        VerQueryValueW(data.as_ptr() as *const c_void, PCWSTR(query.as_ptr()), &mut ptr, &mut len)
    }
    .as_bool();
    if !found || ptr.is_null() || len == 0 {
        return None;
    }
    // SAFETY: VerQueryValueW reports `len` UTF-16 units at ptr, inside data.
    let units = unsafe { std::slice::from_raw_parts(ptr as *const u16, len as usize) };
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..end])).filter(|s| !s.is_empty())
}

/// Display name for a window's group: AppsFolder name for an AUMID, else the
/// exe's FileDescription, else the window title. Cached per identity.
fn display_name(key: &str, aumid: Option<&str>, exe: Option<&str>, title: &str) -> String {
    let cached = locked(names()).get(key).cloned();
    let resolved = match cached {
        Some(name) => name,
        None => {
            let name = aumid
                .and_then(apps_folder_name)
                .or_else(|| exe.and_then(file_description))
                .unwrap_or_default();
            locked(names()).insert(key.to_string(), name.clone());
            name
        }
    };
    if resolved.is_empty() {
        title.to_string()
    } else {
        resolved
    }
}

// ---------------------------------------------------------------------------
// Snapshot: enumerate, identify, merge
// ---------------------------------------------------------------------------

/// Reads the .lnk at `path`: (target path, arguments).
fn read_lnk(path: &str) -> Option<(String, String)> {
    ensure_com();
    // SAFETY: CLSID_ShellLink in-proc; the COM objects are released when dropped.
    let link: IShellLinkW =
        unsafe { CoCreateInstance(&ShellLink, None::<&IUnknown>, CLSCTX_INPROC_SERVER) }.ok()?;
    let file: IPersistFile = link.cast().ok()?;
    let wide = to_wide(path);
    // SAFETY: wide is NUL-terminated and lives across the call.
    unsafe { file.Load(PCWSTR(wide.as_ptr()), STGM_READ) }.ok()?;

    let mut target = vec![0u16; 1024];
    // SAFETY: target is valid for its length; no find data requested.
    unsafe { link.GetPath(&mut target, null_mut(), 0) }.ok()?;
    let mut args = vec![0u16; 4096];
    // SAFETY: args is valid for its length.
    unsafe { link.GetArguments(&mut args) }.ok()?;

    let text = |buf: &[u16]| {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    };
    Some((text(&target), text(&args)))
}

/// (exe the pin starts, .lnk arguments) for a pin's `launch` string.
fn launch_target(launch: &str) -> (Option<String>, String) {
    let lower = launch.to_ascii_lowercase();
    if lower.ends_with(".lnk") {
        let cached = locked(lnk_cache()).get(launch).cloned();
        if let Some(hit) = cached {
            return hit;
        }
        let resolved = match read_lnk(launch) {
            Some((target, args)) => ((!target.is_empty()).then_some(target), args),
            None => (None, String::new()),
        };
        locked(lnk_cache()).insert(launch.to_string(), resolved.clone());
        return resolved;
    }
    if lower.ends_with(".exe") {
        return (Some(launch.to_string()), String::new());
    }
    (None, String::new())
}

fn pin_entry(app: &App) -> PinEntry {
    let (target, args) = launch_target(&app.launch);
    PinEntry {
        id: app.id.clone(),
        name: app.name.clone(),
        launch: app.launch.clone(),
        target,
        args,
    }
}

/// Builds the dock list from the current pins and windows.
fn snapshot() -> Vec<DockItem> {
    ensure_com();
    let pins: Vec<PinEntry> = crate::apps::pinned_apps().iter().map(pin_entry).collect();
    let foreground = to_i64(foreground_window());
    let flashing = locked(state()).flashing.clone();

    let mut procs: HashMap<u32, ProcInfo> = HashMap::new();
    let mut windows: Vec<WindowEntry> = Vec::new();
    for hwnd in top_level_windows() {
        let (facts, pid) = window_facts(hwnd);
        if !is_taskbar_window(&facts) {
            continue;
        }
        let proc = procs.entry(pid).or_insert_with(|| process_info(pid));
        let exe = proc.exe.clone();
        let packaged = proc.packaged_aumid.is_some();
        let aumid = if facts.class == EXPLORER_CLASS {
            Some(EXPLORER_AUMID.to_string())
        } else {
            property_aumid(hwnd).or_else(|| proc.packaged_aumid.clone())
        };
        let h = to_i64(hwnd);
        let key = identity_key(aumid.as_deref(), exe.as_deref(), h);
        let name = display_name(&key, aumid.as_deref(), exe.as_deref(), &facts.title);
        windows.push(WindowEntry {
            hwnd: h,
            title: facts.title,
            aumid,
            exe,
            packaged,
            name,
            foreground: h == foreground,
            flashing: flashing.contains(&h),
        });
    }
    merge(&pins, &windows)
}

// ---------------------------------------------------------------------------
// Watcher thread
// ---------------------------------------------------------------------------

thread_local! {
    /// Watcher state. Only the watcher thread touches it.
    static WATCH: RefCell<Option<Watch>> = const { RefCell::new(None) };
}

struct Watch {
    window: HWND,
    /// Delay in ms of the debounce timer that is armed, if any.
    armed: Option<u32>,
    /// Registered message id for the shell hook.
    shell_msg: u32,
}

/// Starts the watcher thread. Returns immediately.
pub fn start(app: AppHandle) {
    let _ = APP.set(app);
    if let Err(err) = std::thread::Builder::new()
        .name("lucent-tasks".into())
        .spawn(watch_thread)
    {
        eprintln!("lucent: task watcher did not start: {err}");
    }
}

fn watch_thread() {
    ensure_com();
    let Some(window) = create_watch_window() else {
        eprintln!("lucent: task watcher window failed");
        return;
    };
    // SAFETY: registers a constant message name; the id is only used by this thread.
    let shell_msg = unsafe { RegisterWindowMessageW(w!("SHELLHOOK")) };
    // SAFETY: window is our own live top-level window, which lives until this thread exits.
    let _ = unsafe { RegisterShellHookWindow(window) };
    WATCH.with(|w| {
        *w.borrow_mut() = Some(Watch {
            window,
            armed: None,
            shell_msg,
        })
    });

    let mut hooks: Vec<HWINEVENTHOOK> = Vec::new();
    for &(min, max) in HOOKED_EVENTS.iter() {
        // SAFETY: out-of-context hooks; callbacks run on this thread's message loop, which is below.
        let hook = unsafe {
            SetWinEventHook(min, max, None, Some(on_win_event), 0, 0, WINEVENT_OUTOFCONTEXT)
        };
        if !hook.0.is_null() {
            hooks.push(hook);
        }
    }

    // Build the first list.
    schedule(DEBOUNCE_MS);

    // Hooks and the watcher window are dispatched from this loop.
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out pointer for this thread's queue.
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if result <= 0 {
            break;
        }
        // SAFETY: msg was filled by GetMessageW.
        unsafe {
            DispatchMessageW(&msg);
        }
    }

    for hook in hooks {
        // SAFETY: each hook was installed above on this thread and is removed once.
        let _ = unsafe { UnhookWinEvent(hook) };
    }
}

/// Hidden top-level window that receives timer and shell hook messages.
fn create_watch_window() -> Option<HWND> {
    // SAFETY: GetModuleHandleW with a null name returns this process's module.
    let instance = unsafe { GetModuleHandleW(PCWSTR::null()) }.ok()?;
    let class = w!("LucentTasksWatch");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(watch_proc),
        hInstance: instance.into(),
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: wc and the class name outlive the call. A duplicate registration returns 0 and is harmless.
    unsafe { RegisterClassW(&wc) };
    // SAFETY: creates a never-shown popup with a static class.
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
            Some(instance.into()),
            None,
        )
    }
    .ok()
}

unsafe extern "system" fn watch_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let shell_msg = WATCH.with(|w| w.borrow().as_ref().map(|s| s.shell_msg));
    if msg == WM_TIMER && wparam.0 == TIMER_ID {
        fire(hwnd);
        return LRESULT(0);
    }
    if shell_msg == Some(msg) {
        // For HSHELL_* codes lParam is the window concerned.
        if is_flash_message(wparam.0) {
            note_flash(FlashEvent::Flash(lparam.0 as i64));
            schedule(debounce_delay(false));
        }
        return LRESULT(0);
    }
    // SAFETY: forwarding an unhandled message to the default procedure with its own arguments.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// WinEvent callback. Keeps window-level events only.
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
    match event {
        EVENT_SYSTEM_FOREGROUND => note_flash(FlashEvent::Foreground(to_i64(hwnd))),
        EVENT_OBJECT_DESTROY => note_flash(FlashEvent::Destroyed(to_i64(hwnd))),
        _ => {}
    }
    schedule(debounce_delay(event == EVENT_OBJECT_NAMECHANGE));
}

/// Arms the one-shot debounce timer for `delay` ms. A timer already armed is
/// kept, so events that arrive while it runs are absorbed, unless it is armed
/// for longer than `delay` (a non-NAMECHANGE event after a NAMECHANGE one): then
/// it is re-armed sooner.
fn schedule(delay: u32) {
    WATCH.with(|w| {
        let mut guard = w.borrow_mut();
        let Some(slot) = guard.as_mut() else {
            return;
        };
        if !should_rearm(slot.armed, delay) {
            return;
        }
        // SAFETY: slot.window is this thread's live watcher window; the timer posts WM_TIMER to it. SetTimer with an existing id replaces that timer.
        let ok = unsafe { SetTimer(Some(slot.window), TIMER_ID, delay, None) };
        slot.armed = (ok != 0).then_some(delay);
    });
}

/// Debounce timer fired: disarm it, rebuild, and publish.
fn fire(hwnd: HWND) {
    // SAFETY: hwnd is the watcher window on this thread; disarms our own timer.
    let _ = unsafe { KillTimer(Some(hwnd), TIMER_ID) };
    WATCH.with(|w| {
        if let Some(slot) = w.borrow_mut().as_mut() {
            slot.armed = None;
        }
    });
    publish(snapshot());
}

// ---------------------------------------------------------------------------
// Commands and the CLI probe
// ---------------------------------------------------------------------------

/// `lucent --print-dock`: the current dock items as JSON.
pub fn print_probe() {
    let items = snapshot();
    match serde_json::to_string(&items) {
        Ok(text) => println!("{text}"),
        Err(err) => eprintln!("lucent: dock JSON failed: {err}"),
    }
}

#[tauri::command]
pub fn get_dock() -> Vec<DockItem> {
    let cached = locked(state()).items.clone();
    if let Some(items) = cached {
        return items;
    }
    // Nothing has been built yet: build once, synchronously.
    let items = snapshot();
    publish(items.clone());
    items
}

/// Checks that `hwnd` is still a window. Returns it as an HWND.
fn checked_window(hwnd: i64) -> Result<HWND, String> {
    let target = from_i64(hwnd);
    // SAFETY: IsWindow only validates the handle value.
    if unsafe { IsWindow(Some(target)) }.as_bool() {
        Ok(target)
    } else {
        Err(format!("window {hwnd} no longer exists"))
    }
}

/// Presses and releases Alt. Windows lets a process that has just received
/// input bring its window to the front (the foreground lock).
fn alt_tap() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_MENU,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    // SAFETY: inputs is a valid slice of INPUT, and its size is passed as cbSize.
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

/// A window the commands may act on: it must be listed in the current dock
/// list, and still exist. Anything else is refused.
fn require_dock_window(hwnd: i64) -> Result<HWND, String> {
    let listed = locked(state())
        .items
        .as_deref()
        .is_some_and(|items| is_dock_window(items, hwnd));
    if !listed {
        return Err(format!("window {hwnd} is not on the dock"));
    }
    checked_window(hwnd)
}

#[tauri::command]
pub fn activate_window(hwnd: i64) -> Result<(), String> {
    let target = require_dock_window(hwnd)?;
    // SAFETY: read-only window state queries on a validated handle.
    let is_foreground = unsafe { GetForegroundWindow() } == target;
    // SAFETY: read-only query on a validated handle.
    let is_minimized = unsafe { IsIconic(target) }.as_bool();
    match activation_for(is_foreground, is_minimized) {
        Activation::Minimize => {
            // SAFETY: validated handle; ShowWindowAsync posts the request to the owning thread, so a hung target cannot block us.
            let _ = unsafe { ShowWindowAsync(target, SW_MINIMIZE) };
        }
        Activation::Bring { restore } => {
            if restore {
                // SAFETY: validated handle; ShowWindowAsync posts the restore request, so a hung target cannot block us.
                let _ = unsafe { ShowWindowAsync(target, SW_RESTORE) };
            }
            alt_tap();
            // SAFETY: validated handle. The BOOL is not an error signal here: a refused foreground change still leaves the window flashing on the taskbar.
            let _ = unsafe { SetForegroundWindow(target) };
        }
    }
    Ok(())
}

#[tauri::command]
pub fn close_window(hwnd: i64) -> Result<(), String> {
    let target = require_dock_window(hwnd)?;
    // SAFETY: validated handle; WM_CLOSE asks the window to close itself.
    unsafe { PostMessageW(Some(target), WM_CLOSE, WPARAM(0), LPARAM(0)) }
        .map_err(|err| format!("close failed: {err}"))
}

// ---------------------------------------------------------------------------
// Tests (pure functions only: nothing here touches a real window)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(ex_style: u32, has_owner: bool) -> WindowFacts {
        WindowFacts {
            visible: true,
            cloaked: false,
            title: "Document".into(),
            has_owner,
            ex_style,
            class: "Notepad".into(),
            same_process: false,
        }
    }

    fn pin(id: &str, name: &str, target: Option<&str>, args: &str) -> PinEntry {
        PinEntry {
            id: id.into(),
            name: name.into(),
            launch: format!("C:\\pins\\{name}.lnk"),
            target: target.map(String::from),
            args: args.into(),
        }
    }

    fn win(hwnd: i64, aumid: Option<&str>, exe: Option<&str>, name: &str) -> WindowEntry {
        WindowEntry {
            hwnd,
            title: format!("{name} window {hwnd}"),
            aumid: aumid.map(String::from),
            exe: exe.map(String::from),
            packaged: false,
            name: name.into(),
            foreground: false,
            flashing: false,
        }
    }

    const CHROME: &str = "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe";
    const DISCORD_EXE: &str = "C:\\Users\\alex\\AppData\\Local\\Discord\\app-1.0.9\\Discord.exe";

    #[test]
    fn plain_app_window_is_eligible() {
        assert!(is_taskbar_window(&facts(0, false)));
    }

    #[test]
    fn tool_window_is_not_eligible_unless_app_window() {
        assert!(!is_taskbar_window(&facts(WS_EX_TOOLWINDOW.0, false)));
        assert!(is_taskbar_window(&facts(
            WS_EX_TOOLWINDOW.0 | WS_EX_APPWINDOW.0,
            false
        )));
    }

    #[test]
    fn owned_window_is_eligible_only_as_app_window() {
        assert!(!is_taskbar_window(&facts(0, true)));
        assert!(is_taskbar_window(&facts(WS_EX_APPWINDOW.0, true)));
    }

    #[test]
    fn noactivate_window_is_not_eligible_unless_app_window() {
        assert!(!is_taskbar_window(&facts(WS_EX_NOACTIVATE.0, false)));
        assert!(is_taskbar_window(&facts(
            WS_EX_NOACTIVATE.0 | WS_EX_APPWINDOW.0,
            false
        )));
    }

    #[test]
    fn invisible_cloaked_untitled_or_own_windows_are_not_eligible() {
        let mut f = facts(0, false);
        f.visible = false;
        assert!(!is_taskbar_window(&f));

        let mut f = facts(0, false);
        f.cloaked = true;
        assert!(!is_taskbar_window(&f));

        let mut f = facts(0, false);
        f.title.clear();
        assert!(!is_taskbar_window(&f));

        let mut f = facts(0, false);
        f.same_process = true;
        assert!(!is_taskbar_window(&f));
    }

    #[test]
    fn shell_classes_are_not_eligible() {
        for class in ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"] {
            let mut f = facts(0, false);
            f.class = class.into();
            assert!(!is_taskbar_window(&f), "{class} must be excluded");
        }
    }

    #[test]
    fn pin_matches_by_aumid_case_insensitively() {
        let p = pin("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App", "Calculator", None, "");
        let w = win(
            1,
            Some("microsoft.windowscalculator_8wekyb3d8bbwe!app"),
            Some("C:\\Program Files\\WindowsApps\\calc.exe"),
            "Calculator",
        );
        assert!(pin_matches(&p, &w));
    }

    #[test]
    fn pin_matches_by_exe_path_case_insensitively() {
        let p = pin("chrome", "Google Chrome", Some(CHROME), "");
        let w = win(2, None, Some(&CHROME.to_uppercase()), "Google Chrome");
        assert!(pin_matches(&p, &w));
    }

    #[test]
    fn discord_update_exe_pin_matches_by_lnk_arguments() {
        // Pin target is Update.exe; its arguments name the real Discord.exe.
        let p = pin(
            "c:\\users\\alex\\appdata\\local\\discord\\update.exe",
            "Discord Stable",
            Some("C:\\Users\\alex\\AppData\\Local\\Discord\\Update.exe"),
            "--processStart Discord.exe",
        );
        assert!(pin_matches(&p, &win(3, None, Some(DISCORD_EXE), "Discord")));
        // A different app must not take the pin.
        let other = "C:\\Program Files\\Other\\Discordia.exe";
        assert!(!pin_matches(&p, &win(4, None, Some(other), "Discordia")));
    }

    #[test]
    fn arguments_match_whole_tokens_only() {
        assert!(args_mention("--processStart Discord.exe", "Discord.exe"));
        assert!(args_mention("--processStart=\"Discord.exe\"", "Discord.exe"));
        assert!(!args_mention("--processStart DiscordCanary.exe", "Discord.exe"));
        assert!(!args_mention("", "Discord.exe"));
        assert!(!args_mention("--processStart Discord.exe", ""));
    }

    #[test]
    fn pin_matches_by_exe_stem_or_name() {
        let spotify = pin("spotify-id", "Spotify", None, "");
        assert!(pin_matches(
            &spotify,
            &win(5, None, Some("C:\\Apps\\spotify.exe"), "Spotify")
        ));
        // Stem must match exactly, not as a prefix.
        assert!(!pin_matches(
            &spotify,
            &win(6, None, Some("C:\\Apps\\Spotify Helper.exe"), "Helper")
        ));
        // Window without an exe cannot match by stem.
        assert!(!pin_matches(&spotify, &win(7, None, None, "Spotify")));
    }

    #[test]
    fn pin_matches_by_target_stem() {
        // Pin named differently from its exe, matched by the target's stem.
        let p = pin("id", "Google Chrome", Some(CHROME), "");
        assert!(pin_matches(&p, &win(8, None, Some("D:\\Other\\chrome.exe"), "Chrome")));
    }

    #[test]
    fn unrelated_window_does_not_match_pin() {
        let p = pin("notepad-id", "Notepad", Some("C:\\Windows\\notepad.exe"), "");
        assert!(!pin_matches(
            &p,
            &win(9, Some("Microsoft.VSCode"), Some("C:\\Code\\Code.exe"), "Code")
        ));
    }

    #[test]
    fn identity_prefers_aumid_then_exe_then_hwnd() {
        assert_eq!(identity_key(Some("App.Id"), Some("C:\\x.exe"), 1), "app.id");
        assert_eq!(identity_key(None, Some("C:\\Dir\\X.EXE"), 1), "c:\\dir\\x.exe");
        assert_eq!(identity_key(Some(""), None, 42), "hwnd:42");
    }

    #[test]
    fn file_stem_strips_directory_and_extension() {
        assert_eq!(file_stem("C:\\a\\b\\Discord.exe"), "Discord");
        assert_eq!(file_stem("C:\\a\\Update"), "Update");
        assert_eq!(file_name("C:/a/b/c.exe"), "c.exe");
    }

    #[test]
    fn unpinned_launch_uses_appsfolder_for_packaged_apps_and_exe_otherwise() {
        assert_eq!(
            unpinned_launch(Some("Some.App_123!App"), true, Some("C:\\Program Files\\WindowsApps\\x.exe")),
            "shell:AppsFolder\\Some.App_123!App"
        );
        assert_eq!(
            unpinned_launch(None, false, Some("C:\\Tools\\tool.exe")),
            "C:\\Tools\\tool.exe"
        );
        assert_eq!(
            unpinned_launch(Some("X!App"), false, Some("C:\\Program Files\\WindowsApps\\y.exe")),
            "shell:AppsFolder\\X!App"
        );
        assert_eq!(unpinned_launch(Some("X!App"), false, None), "shell:AppsFolder\\X!App");
        assert_eq!(unpinned_launch(None, false, None), "");
    }

    #[test]
    fn merge_puts_pins_first_then_unpinned_groups_in_first_seen_order() {
        let pins = vec![
            pin("chrome", "Google Chrome", Some(CHROME), ""),
            pin("discord", "Discord", Some("C:\\D\\Update.exe"), "--processStart Discord.exe"),
            pin("spotify", "Spotify", None, ""),
        ];
        let mut chrome1 = win(10, None, Some(CHROME), "Google Chrome");
        chrome1.foreground = true;
        let mut chrome2 = win(11, None, Some(CHROME), "Google Chrome");
        chrome2.flashing = true;
        let foo1 = win(12, None, Some("C:\\Tools\\foo.exe"), "Foo");
        let discord = win(13, None, Some(DISCORD_EXE), "Discord");
        let foo2 = win(14, None, Some("C:\\Tools\\foo.exe"), "Foo");
        // z-order: chrome1, foo1, chrome2, discord, foo2
        let windows = vec![chrome1, foo1, chrome2, discord, foo2];

        let items = merge(&pins, &windows);
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            ["chrome", "discord", "spotify", "c:\\tools\\foo.exe"]
        );

        let chrome = &items[0];
        assert!(chrome.pinned);
        assert!(chrome.active, "a member is foreground");
        assert!(chrome.flashing, "a member is flashing");
        assert_eq!(
            chrome.windows.iter().map(|w| w.hwnd).collect::<Vec<_>>(),
            [10, 11],
            "windows keep z-order"
        );

        assert_eq!(items[1].windows.len(), 1);
        assert!(!items[1].active && !items[1].flashing);

        let spotify = &items[2];
        assert!(spotify.pinned && spotify.windows.is_empty(), "pin without windows stays listed");

        let foo = &items[3];
        assert!(!foo.pinned);
        assert_eq!(foo.launch, "C:\\Tools\\foo.exe");
        assert_eq!(foo.windows.iter().map(|w| w.hwnd).collect::<Vec<_>>(), [12, 14]);
        assert_eq!(foo.name, "Foo");
    }

    #[test]
    fn window_goes_to_the_first_pin_that_matches() {
        let pins = vec![
            pin("first", "Notes", None, ""),
            pin("second", "Notes", None, ""),
        ];
        let windows = vec![win(20, None, Some("C:\\n\\notes.exe"), "Notes")];
        let items = merge(&pins, &windows);
        assert_eq!(items[0].windows.len(), 1);
        assert!(items[1].windows.is_empty());
        assert_eq!(items.len(), 2, "no unpinned duplicate");
    }

    #[test]
    fn unpinned_name_falls_back_to_title_when_empty() {
        let mut w = win(30, None, Some("C:\\x\\y.exe"), "");
        w.title = "Untitled - Thing".into();
        let items = merge(&[], &[w]);
        assert_eq!(items[0].name, "Untitled - Thing");
        assert!(!items[0].pinned);
    }

    #[test]
    fn activation_decision_table() {
        assert_eq!(activation_for(true, false), Activation::Minimize);
        assert_eq!(activation_for(true, true), Activation::Minimize);
        assert_eq!(activation_for(false, true), Activation::Bring { restore: true });
        assert_eq!(activation_for(false, false), Activation::Bring { restore: false });
    }

    #[test]
    fn flashing_starts_on_flash_and_stops_on_foreground_or_destroy() {
        let mut set = HashSet::new();
        apply_flash_event(&mut set, FlashEvent::Flash(1));
        apply_flash_event(&mut set, FlashEvent::Flash(2));
        assert_eq!(set.len(), 2);

        apply_flash_event(&mut set, FlashEvent::Foreground(1));
        assert!(!set.contains(&1) && set.contains(&2));

        apply_flash_event(&mut set, FlashEvent::Destroyed(2));
        assert!(set.is_empty());

        // Removing a window that is not flashing changes nothing.
        apply_flash_event(&mut set, FlashEvent::Foreground(99));
        assert!(set.is_empty());
    }

    #[test]
    fn shell_hook_flash_code_is_recognised_in_low_word_only() {
        assert!(is_flash_message(0x8006));
        assert!(is_flash_message(0x1_0000 | 0x8006));
        assert!(!is_flash_message(0x8005));
        assert!(!is_flash_message(1));
    }

    #[test]
    fn only_windows_listed_in_the_dock_are_actionable() {
        let item = |id: &str, hwnds: &[i64]| DockItem {
            id: id.into(),
            name: id.into(),
            launch: String::new(),
            pinned: false,
            windows: hwnds
                .iter()
                .map(|&hwnd| DockWindow {
                    hwnd,
                    title: String::new(),
                })
                .collect(),
            active: false,
            flashing: false,
        };
        let items = vec![item("a", &[10, 11]), item("pinned-empty", &[]), item("b", &[20])];
        assert!(is_dock_window(&items, 10));
        assert!(is_dock_window(&items, 20));
        assert!(!is_dock_window(&items, 12), "unknown hwnd is refused");
        assert!(!is_dock_window(&items, 0));
        assert!(!is_dock_window(&[], 10), "no list means nothing is actionable");
    }

    #[test]
    fn namechange_storms_use_the_longer_debounce() {
        assert_eq!(debounce_delay(true), 500);
        assert_eq!(debounce_delay(false), 100);
    }

    #[test]
    fn armed_timer_is_only_replaced_by_a_sooner_one() {
        assert!(should_rearm(None, 500), "nothing armed: arm");
        assert!(!should_rearm(Some(500), 500), "a namechange during a namechange timer is absorbed");
        assert!(!should_rearm(Some(100), 500), "a namechange never delays a sooner timer");
        assert!(should_rearm(Some(500), 100), "a real event shortens a namechange timer");
    }

    #[test]
    fn publish_only_replaces_on_change() {
        let item = DockItem {
            id: "a".into(),
            name: "A".into(),
            launch: "a.exe".into(),
            pinned: false,
            windows: vec![],
            active: false,
            flashing: false,
        };
        let mut slot = None;
        assert!(replace_if_changed(&mut slot, vec![item.clone()]), "first build is a change");
        assert!(!replace_if_changed(&mut slot, vec![item.clone()]), "same list is not");
        let mut moved = item;
        moved.active = true;
        assert!(replace_if_changed(&mut slot, vec![moved]));
    }
}

//! Notification-area host. Lucent creates its own top-level `Shell_TrayWnd`
//! above Explorer's, so `Shell_NotifyIcon` calls (which go to the first
//! `Shell_TrayWnd` in z-order) reach Lucent first. Every message is then
//! forwarded unchanged to Explorer's tray, so Explorer keeps a full copy of the
//! icons and is intact when Lucent quits. See docs/taskbar-spec.md, module `tray`.
//!
//! Everything Win32 runs on one dedicated thread with its own message loop. No
//! polling: the window is re-raised only when Explorer shows or activates a
//! tray window, or when Explorer restarts.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW,
    DispatchMessageW, FindWindowExW, KillTimer, SetTimer, GetClassNameW, GetCursorPos, GetIconInfo, GetMessageW,
    GetWindowRect, GetWindowThreadProcessId, InSendMessage, IsWindow, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SendMessageTimeoutW, SetWindowPos,
    EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, HICON, HWND_BROADCAST, HWND_TOPMOST, ICONINFO,
    MSG, MSGFLT_ALLOW, OBJID_WINDOW, SMTO_ABORTIFHUNG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOOWNERZORDER, WINEVENT_OUTOFCONTEXT, WM_CLOSE, WM_COMMAND, WM_COPYDATA,
    WM_DESTROY, WM_TIMER, WM_USER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::apps;

/// Event sent to every window when the icon list changes. Payload: `Vec<IconView>`.
pub const CHANGED_EVENT: &str = "tray-changed";

// ---------------------------------------------------------------------------
// The wire format: what shell32 sends in WM_COPYDATA.

/// `COPYDATASTRUCT::dwData` for Shell_NotifyIcon. (0 is SHAppBarMessage, 3 is
/// Shell_NotifyIconGetRect; both are forwarded untouched.)
pub const COPYDATA_NOTIFY: usize = 1;
/// First DWORD of the payload.
pub const TRAY_SIGNATURE: u32 = 0x3475_3423;

pub const NIM_ADD: u32 = 0;
pub const NIM_MODIFY: u32 = 1;
pub const NIM_DELETE: u32 = 2;
pub const NIM_SETFOCUS: u32 = 3;
pub const NIM_SETVERSION: u32 = 4;

pub const NIF_MESSAGE: u32 = 0x01;
pub const NIF_ICON: u32 = 0x02;
pub const NIF_TIP: u32 = 0x04;
pub const NIF_STATE: u32 = 0x08;
pub const NIF_GUID: u32 = 0x20;

pub const NIS_HIDDEN: u32 = 0x01;

// Byte offsets in the payload: a signature and NIM_* command, then a
// NOTIFYICONDATAW whose handles are 32-bit, so 32- and 64-bit callers agree.
const OFF_SIGNATURE: usize = 0;
const OFF_COMMAND: usize = 4;
const OFF_HWND: usize = 12;
const OFF_UID: usize = 16;
const OFF_FLAGS: usize = 20;
const OFF_CALLBACK: usize = 24;
const OFF_HICON: usize = 28;
const OFF_TIP: usize = 32;
const TIP_CHARS: usize = 128;
const OFF_STATE: usize = 288;
const OFF_STATE_MASK: usize = 292;
const OFF_VERSION: usize = 808;
const OFF_GUID: usize = 944;
/// The shortest payload that still has every field up to and including hIcon.
const MIN_PAYLOAD: usize = OFF_TIP;

/// One decoded Shell_NotifyIcon call. Handles stay 32-bit, as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayMessage {
    pub command: u32,
    pub hwnd: u32,
    pub uid: u32,
    pub flags: u32,
    pub callback: u32,
    pub hicon: u32,
    pub tip: String,
    pub state: u32,
    pub state_mask: u32,
    pub version: u32,
    pub guid: Option<[u8; 16]>,
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let chunk = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(<[u8; 4]>::try_from(chunk).ok()?))
}

/// Reads a NUL-terminated UTF-16 string of at most `chars` units. Whatever part
/// of it lies inside `bytes` is used.
fn read_wstr(bytes: &[u8], at: usize, chars: usize) -> String {
    let end = at.saturating_add(chars * 2).min(bytes.len());
    let units: Vec<u16> = bytes
        .get(at..end)
        .unwrap_or(&[])
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Decodes a Shell_NotifyIcon WM_COPYDATA payload. None when it is too short or
/// not signed as tray data. Fields past the end of a short payload read as 0.
pub fn parse_tray_data(bytes: &[u8]) -> Option<TrayMessage> {
    if bytes.len() < MIN_PAYLOAD || read_u32(bytes, OFF_SIGNATURE)? != TRAY_SIGNATURE {
        return None;
    }
    let flags = read_u32(bytes, OFF_FLAGS)?;
    let guid = bytes
        .get(OFF_GUID..OFF_GUID + 16)
        .and_then(|raw| <[u8; 16]>::try_from(raw).ok())
        .filter(|raw| flags & NIF_GUID != 0 && raw.iter().any(|&b| b != 0));
    Some(TrayMessage {
        command: read_u32(bytes, OFF_COMMAND)?,
        hwnd: read_u32(bytes, OFF_HWND)?,
        uid: read_u32(bytes, OFF_UID)?,
        flags,
        callback: read_u32(bytes, OFF_CALLBACK)?,
        hicon: read_u32(bytes, OFF_HICON)?,
        tip: read_wstr(bytes, OFF_TIP, TIP_CHARS),
        state: read_u32(bytes, OFF_STATE).unwrap_or(0),
        state_mask: read_u32(bytes, OFF_STATE_MASK).unwrap_or(0),
        version: read_u32(bytes, OFF_VERSION).unwrap_or(0),
        guid,
    })
}

// ---------------------------------------------------------------------------
// The icon store (pure).

/// One icon the host knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayIcon {
    pub id: u64,
    pub hwnd: u32,
    pub uid: u32,
    pub guid: Option<[u8; 16]>,
    pub callback: u32,
    pub version: u32,
    pub tip: String,
    /// PNG data URL. None until an icon arrives, or when it could not be read.
    pub image: Option<String>,
    pub state: u32,
    /// The owning app's name, the label when there is no tooltip.
    pub app: Option<String>,
}

impl TrayIcon {
    fn matches(&self, msg: &TrayMessage) -> bool {
        match msg.guid {
            Some(guid) => self.guid == Some(guid),
            None => self.guid.is_none() && self.hwnd == msg.hwnd && self.uid == msg.uid,
        }
    }

    pub fn visible(&self) -> bool {
        self.state & NIS_HIDDEN == 0
    }

    /// Copies the fields that `msg.flags` says are valid. `image` is the
    /// rendered `msg.hicon`, used only with NIF_ICON.
    fn update(&mut self, msg: &TrayMessage, image: &Option<String>) {
        if msg.hwnd != 0 {
            self.hwnd = msg.hwnd;
            self.uid = msg.uid;
        }
        if msg.flags & NIF_MESSAGE != 0 {
            self.callback = msg.callback;
        }
        if msg.flags & NIF_ICON != 0 {
            self.image = image.clone();
        }
        if msg.flags & NIF_TIP != 0 {
            self.tip = msg.tip.clone();
        }
        if msg.flags & NIF_STATE != 0 {
            self.state = (self.state & !msg.state_mask) | (msg.state & msg.state_mask);
        }
    }
}

#[derive(Debug, Default)]
pub struct Store {
    pub icons: Vec<TrayIcon>,
    next_id: u64,
}

/// What applying a message did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// The return value for the caller of Shell_NotifyIcon.
    pub ok: bool,
    /// Whether anything the panel shows changed.
    pub changed: bool,
}

impl Store {
    fn find(&mut self, msg: &TrayMessage) -> Option<&mut TrayIcon> {
        self.icons.iter_mut().find(|icon| icon.matches(msg))
    }

    /// Applies one Shell_NotifyIcon call with NOTIFYICON_VERSION_4 semantics.
    /// A repeated NIM_ADD updates the existing icon (apps re-add after
    /// TaskbarCreated). A NIM_MODIFY with an icon for an unknown entry adds it,
    /// so icons registered before Lucent started appear on their next update.
    pub fn apply(&mut self, msg: &TrayMessage, image: Option<String>) -> Applied {
        match msg.command {
            NIM_ADD | NIM_MODIFY => {
                if let Some(icon) = self.find(msg) {
                    let before = icon.clone();
                    icon.update(msg, &image);
                    let changed = *icon != before;
                    return Applied { ok: true, changed };
                }
                if msg.command == NIM_MODIFY && msg.flags & NIF_ICON == 0 {
                    return Applied { ok: false, changed: false };
                }
                self.next_id += 1;
                let mut icon = TrayIcon {
                    id: self.next_id,
                    hwnd: msg.hwnd,
                    uid: msg.uid,
                    guid: msg.guid,
                    callback: 0,
                    version: 0,
                    tip: String::new(),
                    image: None,
                    state: 0,
                    app: None,
                };
                icon.update(msg, &image);
                self.icons.push(icon);
                Applied { ok: true, changed: true }
            }
            NIM_DELETE => {
                let before = self.icons.len();
                self.icons.retain(|icon| !icon.matches(msg));
                let changed = self.icons.len() != before;
                Applied { ok: changed, changed }
            }
            NIM_SETVERSION => match self.find(msg) {
                Some(icon) => {
                    icon.version = msg.version;
                    Applied { ok: true, changed: false }
                }
                None => Applied { ok: false, changed: false },
            },
            NIM_SETFOCUS => Applied { ok: self.find(msg).is_some(), changed: false },
            _ => Applied { ok: false, changed: false },
        }
    }

    /// Drops icons whose owner window is gone. True when any were dropped.
    pub fn retain_alive(&mut self, alive: impl Fn(u32) -> bool) -> bool {
        let before = self.icons.len();
        self.icons.retain(|icon| alive(icon.hwnd));
        self.icons.len() != before
    }

    /// Names the owners of icons that have no app name yet. True when any changed.
    pub fn fill_names(&mut self, name: impl Fn(u32) -> Option<String>) -> bool {
        let mut changed = false;
        for icon in self.icons.iter_mut().filter(|icon| icon.app.is_none()) {
            icon.app = name(icon.hwnd);
            changed |= icon.app.is_some();
        }
        changed
    }

    pub fn views(&self) -> Vec<IconView> {
        self.icons
            .iter()
            .filter(|icon| icon.visible())
            .map(|icon| IconView {
                id: icon.id.to_string(),
                tip: icon.tip.clone(),
                image: icon.image.clone(),
                app: icon.app.clone().unwrap_or_default(),
            })
            .collect()
    }
}

/// What the panel gets for one icon.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IconView {
    pub id: String,
    pub tip: String,
    pub image: Option<String>,
    pub app: String,
}

// ---------------------------------------------------------------------------
// Forwarding deletes (pure).

/// How long after Lucent's own TaskbarCreated broadcast a delete of an unknown
/// icon is kept from Explorer.
pub const BROADCAST_GRACE: Duration = Duration::from_secs(10);

/// Whether an NIM_DELETE goes on to Explorer. Apps answer TaskbarCreated with a
/// delete and then an add, but some (Steam) only delete. A delete of an icon
/// Lucent never saw, right after Lucent's own broadcast, is that reply: Explorer
/// keeps the icon, so it is still there when Lucent quits.
pub fn forward_delete(known: bool, since_broadcast: Option<Duration>) -> bool {
    known || since_broadcast.is_none_or(|since| since > BROADCAST_GRACE)
}

// ---------------------------------------------------------------------------
// Clicks (pure).

pub const NIN_SELECT: u32 = WM_USER;
const WM_CONTEXTMENU_MSG: u32 = 0x007B;
const WM_LBUTTONDOWN_MSG: u32 = 0x0201;
const WM_LBUTTONUP_MSG: u32 = 0x0202;
const WM_LBUTTONDBLCLK_MSG: u32 = 0x0203;
const WM_RBUTTONDOWN_MSG: u32 = 0x0204;
const WM_RBUTTONUP_MSG: u32 = 0x0205;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Double,
}

pub fn parse_button(name: &str) -> Result<Button, String> {
    match name {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "double" => Ok(Button::Double),
        _ => Err(format!("unknown button `{name}`")),
    }
}

/// The mouse events Explorer sends for a click, in order. Version 3 and up also
/// get NIN_SELECT after a left click and WM_CONTEXTMENU after a right click.
pub fn click_events(button: Button, version: u32) -> Vec<u32> {
    let modern = version >= 3;
    match button {
        Button::Left if modern => vec![WM_LBUTTONDOWN_MSG, WM_LBUTTONUP_MSG, NIN_SELECT],
        Button::Left => vec![WM_LBUTTONDOWN_MSG, WM_LBUTTONUP_MSG],
        Button::Right if modern => vec![WM_RBUTTONDOWN_MSG, WM_RBUTTONUP_MSG, WM_CONTEXTMENU_MSG],
        Button::Right => vec![WM_RBUTTONDOWN_MSG, WM_RBUTTONUP_MSG],
        Button::Double => vec![WM_LBUTTONDBLCLK_MSG, WM_LBUTTONUP_MSG],
    }
}

/// wParam and lParam of a callback message. Version 4: wParam is the anchor
/// point (x low word, y high word) and lParam is the event (low) and uID
/// (high). Older versions: wParam is the uID and lParam the event.
pub fn callback_params(version: u32, uid: u32, event: u32, x: i32, y: i32) -> (usize, isize) {
    if version >= 4 {
        let point = ((y as u32 & 0xFFFF) << 16) | (x as u32 & 0xFFFF);
        let event = ((uid & 0xFFFF) << 16) | (event & 0xFFFF);
        (point as usize, event as i32 as isize)
    } else {
        (uid as usize, event as isize)
    }
}

// ---------------------------------------------------------------------------
// Icon pixels (pure).

/// Converts a top-down BGRA icon bitmap to straight RGBA. Icon colour bitmaps
/// carry straight alpha. When no pixel has alpha, the AND mask decides: a
/// non-zero mask pixel (BGRA from the same GetDIBits) is transparent.
pub fn icon_to_rgba(bgra: &[u8], mask: Option<&[u8]>) -> Vec<u8> {
    let has_alpha = bgra.chunks_exact(4).any(|px| px[3] != 0);
    let mut out = Vec::with_capacity(bgra.len());
    for (i, px) in bgra.chunks_exact(4).enumerate() {
        let alpha = if has_alpha {
            px[3]
        } else {
            match mask.and_then(|m| m.get(i * 4..i * 4 + 3)) {
                Some(m) if m.iter().any(|&b| b != 0) => 0,
                _ => 255,
            }
        };
        if alpha == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            out.extend_from_slice(&[px[2], px[1], px[0], alpha]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Host state.

static STORE: Mutex<Option<Store>> = Mutex::new(None);
static APP: OnceLock<AppHandle> = OnceLock::new();
static HOST_HWND: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// A TaskbarCreated broadcast waits for this much quiet in the tray z-order.
/// Explorer re-raises its tray many times while it settles (after takeover or
/// a restart), and a re-registration during that would go to Explorer.
const SETTLE_MS: u32 = 1000;
const BROADCAST_TIMER: usize = 1;
/// A broadcast is waiting for the z-order to settle.
static BROADCAST_PENDING: AtomicBool = AtomicBool::new(false);
/// When Lucent last broadcast TaskbarCreated.
static BROADCAST_AT: Mutex<Option<Instant>> = Mutex::new(None);

fn store() -> MutexGuard<'static, Option<Store>> {
    STORE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn hwnd_bits(hwnd: HWND) -> isize {
    hwnd.0 as usize as isize
}

fn hwnd_from_bits(bits: isize) -> HWND {
    HWND(bits as usize as *mut c_void)
}

/// A 32-bit handle from the wire, sign-extended as Windows does for 64-bit.
fn hwnd_from_u32(raw: u32) -> HWND {
    HWND(raw as i32 as isize as *mut c_void)
}

fn host_hwnd() -> Option<HWND> {
    let bits = HOST_HWND.load(Ordering::SeqCst);
    (bits != 0).then(|| hwnd_from_bits(bits))
}

fn is_alive(raw: u32) -> bool {
    // SAFETY: IsWindow accepts any value and only reports whether it is a window.
    unsafe { IsWindow(Some(hwnd_from_u32(raw))) }.as_bool()
}

/// `LUCENT_TRAY_DEBUG=1` logs every Shell_NotifyIcon call to stderr.
fn debug_log() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUCENT_TRAY_DEBUG").is_some_and(|v| v == "1"))
}

fn owner_name(raw: u32) -> Option<String> {
    let mut pid = 0u32;
    // SAFETY: GetWindowThreadProcessId accepts any handle; pid is a valid out-pointer.
    unsafe { GetWindowThreadProcessId(hwnd_from_u32(raw), Some(&mut pid as *mut u32)) };
    (pid != 0).then(|| crate::tasks::process_display_name(pid)).flatten()
}

fn emit_changed() {
    let views = match store().as_ref() {
        Some(store) => store.views(),
        None => return,
    };
    if let Some(app) = APP.get() {
        if let Err(err) = app.emit(CHANGED_EVENT, views) {
            eprintln!("lucent: tray event failed: {err}");
        }
    }
}

// ---------------------------------------------------------------------------
// Startup and shutdown.

/// Starts the tray host thread. A failure is logged and leaves the dock using
/// the native tray instead.
pub fn start(app: AppHandle) {
    let _ = APP.set(app);
    let (ready, wait) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("lucent-tray".into())
        .spawn(move || host_thread(ready));
    if let Err(err) = spawned {
        eprintln!("lucent: tray host thread failed: {err}");
        return;
    }
    match wait.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => eprintln!("lucent: tray host unavailable: {err}"),
        Err(_) => eprintln!("lucent: tray host did not start"),
    }
}

/// Closes the host window so Shell_NotifyIcon reaches Explorer again. Waits a
/// short, bounded time for the host thread to handle it.
pub fn stop() {
    let Some(hwnd) = host_hwnd() else { return };
    let mut result = 0usize;
    // SAFETY: hwnd is the host window or an already-destroyed handle, which the
    // call rejects. The timeout bounds the wait; result is a valid out-pointer.
    unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            1000,
            Some(&mut result as *mut usize),
        )
    };
}

fn host_thread(ready: mpsc::Sender<Result<(), String>>) {
    *store() = Some(Store::default());
    let hwnd = match create_host_window() {
        Ok(hwnd) => hwnd,
        Err(err) => {
            *store() = None;
            let _ = ready.send(Err(err));
            return;
        }
    };
    HOST_HWND.store(hwnd_bits(hwnd), Ordering::SeqCst);
    // SAFETY: the name is a static string literal.
    let created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    TASKBAR_CREATED.store(created, Ordering::SeqCst);
    // Apps at lower integrity (sandboxed browsers) must still reach the tray.
    for msg in [WM_COPYDATA, created] {
        // SAFETY: hwnd is the host window owned by this thread.
        if let Err(err) = unsafe { ChangeWindowMessageFilterEx(hwnd, msg, MSGFLT_ALLOW, None) } {
            eprintln!("lucent: tray message filter: {err}");
        }
    }
    let hooks = [EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_REORDER].map(|event| {
        // SAFETY: the callback is a static extern fn, and the hook is out of context,
        // so it runs on this thread's message loop.
        unsafe { SetWinEventHook(event, event, None, Some(on_tray_event), 0, 0, WINEVENT_OUTOFCONTEXT) }
    });
    raise(hwnd);
    let _ = ready.send(Ok(()));
    // Every app with an icon re-registers, now with this window, once Explorer
    // has stopped re-raising its tray.
    schedule_broadcast();

    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a valid out-pointer. GetMessageW returns <= 0 on quit or error.
        if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
            break;
        }
        // SAFETY: msg was just filled in by GetMessageW.
        unsafe { DispatchMessageW(&msg) };
    }

    HOST_HWND.store(0, Ordering::SeqCst);
    for hook in hooks {
        if !hook.0.is_null() {
            // SAFETY: hook was returned by SetWinEventHook above and is unhooked once.
            let _ = unsafe { UnhookWinEvent(hook) };
        }
    }
    *store() = None;
    emit_changed();
}

/// The host window: a hidden, topmost, tool-window popup of class
/// `Shell_TrayWnd`, with Explorer's tray rect so apps that read it get sane numbers.
fn create_host_window() -> Result<HWND, String> {
    // SAFETY: a null name asks for the handle of the current executable.
    let instance = unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(|e| e.to_string())?;
    let class = w!("Shell_TrayWnd");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(host_proc),
        hInstance: instance.into(),
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: wc and the class name are valid for the call. 0 means the class
    // already exists in this process, which CreateWindowExW then reports.
    unsafe { RegisterClassW(&wc) };
    let mut rc = RECT::default();
    if let Some(tray) = explorer_tray() {
        // SAFETY: tray is a window handle and rc a valid out-pointer.
        let _ = unsafe { GetWindowRect(tray, &mut rc) };
    }
    // SAFETY: the class is registered above, and every pointer argument is null or static.
    unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!(""),
            WS_POPUP,
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .map_err(|e| e.to_string())
}

/// Puts the host at the top of the topmost band, ahead of Explorer's tray.
fn raise(hwnd: HWND) {
    // SAFETY: hwnd is the host window. The flags keep its size, position and activation.
    let _ = unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        )
    };
}

/// The `Shell_TrayWnd` that Shell_NotifyIcon would pick right now.
fn first_tray() -> Option<HWND> {
    // SAFETY: the class name is a static literal; null parents mean top-level windows.
    unsafe { FindWindowExW(None, None, w!("Shell_TrayWnd"), PCWSTR::null()) }.ok()
}

/// Explorer's `Shell_TrayWnd`: the first one in z-order that is not ours.
fn explorer_tray() -> Option<HWND> {
    let ours = std::process::id();
    let mut after: Option<HWND> = None;
    loop {
        // SAFETY: `after` is None or a window returned by the previous call.
        let next = unsafe { FindWindowExW(None, after, w!("Shell_TrayWnd"), PCWSTR::null()) }.ok()?;
        let mut pid = 0u32;
        // SAFETY: pid is a valid out-pointer for the call.
        unsafe { GetWindowThreadProcessId(next, Some(&mut pid as *mut u32)) };
        if pid != ours {
            return Some(next);
        }
        after = Some(next);
    }
}

/// Raises the host when Explorer's tray has moved above it. True when it did.
fn ensure_first() -> bool {
    let Some(ours) = host_hwnd() else { return false };
    if first_tray() == Some(ours) {
        return false;
    }
    raise(ours);
    if debug_log() {
        eprintln!("lucent tray: {:?} re-raised the host", Instant::now());
    }
    if BROADCAST_PENDING.load(Ordering::SeqCst) {
        schedule_broadcast();
    }
    true
}

/// (Re)starts the settle timer; WM_TIMER then broadcasts. Re-arming an
/// existing timer restarts its countdown.
fn schedule_broadcast() {
    let Some(hwnd) = host_hwnd() else { return };
    BROADCAST_PENDING.store(true, Ordering::SeqCst);
    // SAFETY: hwnd is the host window; no callback, so WM_TIMER is posted to it.
    if unsafe { SetTimer(Some(hwnd), BROADCAST_TIMER, SETTLE_MS, None) } == 0 {
        BROADCAST_PENDING.store(false, Ordering::SeqCst);
        broadcast_taskbar_created();
    }
}

fn broadcast_taskbar_created() {
    let msg = TASKBAR_CREATED.load(Ordering::SeqCst);
    if msg == 0 {
        return;
    }
    *BROADCAST_AT.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    // SAFETY: a broadcast post with no pointers; it returns at once.
    if let Err(err) = unsafe { PostMessageW(Some(HWND_BROADCAST), msg, WPARAM(0), LPARAM(0)) } {
        eprintln!("lucent: TaskbarCreated broadcast failed: {err}");
    }
}

fn is_tray_class(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    // SAFETY: buf is a live buffer, and GetClassNameW writes at most its length.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == "Shell_TrayWnd"
}

/// EVENT_OBJECT_SHOW / EVENT_SYSTEM_FOREGROUND for a `Shell_TrayWnd`, or any
/// EVENT_OBJECT_REORDER: Explorer may have raised its tray above the host, so
/// check and raise again. The check is one FindWindowExW.
unsafe extern "system" fn on_tray_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // A z-order change names the parent whose children moved, so just check.
    if event == EVENT_OBJECT_REORDER {
        ensure_first();
        return;
    }
    if id_object != OBJID_WINDOW.0 || hwnd.0.is_null() || Some(hwnd) == host_hwnd() {
        return;
    }
    if is_tray_class(hwnd) {
        ensure_first();
    }
}

// ---------------------------------------------------------------------------
// The window procedure.

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let created = TASKBAR_CREATED.load(Ordering::SeqCst);
    if created != 0 && msg == created {
        // Explorer restarted, and its new tray is above the host. Our own
        // broadcast arrives while the host is first, so it does not loop.
        if ensure_first() {
            schedule_broadcast();
        }
        return LRESULT(0);
    }
    match msg {
        WM_COPYDATA => {
            // SAFETY: for WM_COPYDATA, lparam is a COPYDATASTRUCT that the system
            // keeps valid until this procedure returns.
            let ours = unsafe { on_copydata(lparam) };
            let forwarded = match ours {
                Some(Outcome { forward: false, .. }) => None,
                _ => forward_send(msg, wparam, lparam),
            };
            if debug_log() {
                // SAFETY: as above, lparam is the live COPYDATASTRUCT.
                let cds = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
                eprintln!(
                    "lucent tray: copydata kind={} bytes={} ours={ours:?} explorer={:?}",
                    cds.dwData,
                    cds.cbData,
                    forwarded.map(|r| r.0)
                );
            }
            // The caller hears success if either side accepted the call.
            if ours.is_some_and(|o| o.ok || !o.forward) {
                LRESULT(1)
            } else {
                forwarded.unwrap_or(LRESULT(0))
            }
        }
        WM_TIMER if wparam.0 == BROADCAST_TIMER => {
            // SAFETY: the timer was set on this window by schedule_broadcast.
            let _ = unsafe { KillTimer(Some(hwnd), BROADCAST_TIMER) };
            BROADCAST_PENDING.store(false, Ordering::SeqCst);
            ensure_first();
            broadcast_taskbar_created();
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: called on the thread that owns the window.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // Commands and private messages that apps send to "the taskbar".
        _ if msg == WM_COMMAND || (WM_USER..0x8000).contains(&msg) => {
            // SAFETY: InSendMessage takes no arguments.
            if unsafe { InSendMessage() }.as_bool() {
                forward_send(msg, wparam, lparam).unwrap_or(LRESULT(0))
            } else {
                if let Some(tray) = explorer_tray() {
                    // SAFETY: a post carries the values as they arrived.
                    let _ = unsafe { PostMessageW(Some(tray), msg, wparam, lparam) };
                    ensure_first();
                }
                LRESULT(0)
            }
        }
        // SAFETY: the arguments are the ones Windows passed to this window procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Sends a message on to Explorer's tray. None when there is no Explorer tray
/// or it did not answer in time.
fn forward_send(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let tray = explorer_tray()?;
    let mut result = 0usize;
    // SAFETY: for WM_COPYDATA, lparam is still the live COPYDATASTRUCT (the caller
    // is inside the window procedure), and the system copies it to Explorer. Other
    // messages carry plain values. The timeout bounds the wait.
    let sent = unsafe {
        SendMessageTimeoutW(tray, msg, wparam, lparam, SMTO_ABORTIFHUNG, 4000, Some(&mut result as *mut usize))
    };
    // Explorer re-raises its tray while handling some messages (an NIM_DELETE,
    // for one). Without this, the app's next call (the NIM_ADD that follows a
    // delete) would go straight to Explorer.
    ensure_first();
    (sent.0 != 0).then_some(LRESULT(result as isize))
}

/// What the host did with a Shell_NotifyIcon call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outcome {
    /// The call succeeded for the host.
    ok: bool,
    /// The call goes on to Explorer.
    forward: bool,
}

/// Handles a Shell_NotifyIcon payload. None when it is some other WM_COPYDATA.
///
/// # Safety
/// `lparam` must be the COPYDATASTRUCT pointer of a WM_COPYDATA being handled.
unsafe fn on_copydata(lparam: LPARAM) -> Option<Outcome> {
    // SAFETY: the caller guarantees lparam is a live COPYDATASTRUCT.
    let cds = unsafe { (lparam.0 as *const COPYDATASTRUCT).as_ref() }?;
    if cds.dwData != COPYDATA_NOTIFY || cds.lpData.is_null() {
        return None;
    }
    // SAFETY: lpData points at cbData bytes that stay valid during the message.
    let bytes = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
    let msg = parse_tray_data(bytes)?;
    // The handle is only guaranteed to live while the caller waits on this send,
    // so it is rendered now.
    let image = if msg.flags & NIF_ICON != 0 { render_icon(msg.hicon) } else { None };
    if debug_log() {
        eprintln!(
            "lucent tray: {:?} cmd={} hwnd={:#x} uid={} flags={:#x} cb={:#x} icon={:#x}->{} guid={} ver={} state={:#x}/{:#x} tip={:?} owner={:?}",
            Instant::now(),
            msg.command,
            msg.hwnd,
            msg.uid,
            msg.flags,
            msg.callback,
            msg.hicon,
            image.is_some(),
            msg.guid.is_some(),
            msg.version,
            msg.state,
            msg.state_mask,
            msg.tip,
            owner_name(msg.hwnd)
        );
    }
    let applied = match store().as_mut() {
        Some(store) => {
            let applied = store.apply(&msg, image);
            store.fill_names(owner_name);
            applied
        }
        None => return Some(Outcome { ok: false, forward: true }),
    };
    if applied.changed {
        emit_changed();
    }
    let forward = msg.command != NIM_DELETE || {
        let at = *BROADCAST_AT.lock().unwrap_or_else(PoisonError::into_inner);
        forward_delete(applied.ok, at.map(|at| at.elapsed()))
    };
    Some(Outcome { ok: applied.ok, forward })
}

/// Renders a 32-bit HICON as a PNG data URL at its own size.
fn render_icon(raw: u32) -> Option<String> {
    if raw == 0 {
        return None;
    }
    let hicon = HICON(raw as i32 as isize as *mut c_void);
    let mut info = ICONINFO::default();
    // SAFETY: info is a valid out-pointer. On success the two bitmaps are ours to delete.
    unsafe { GetIconInfo(hicon, &mut info) }.ok()?;
    let color = info.hbmColor;
    let mask = info.hbmMask;
    let pixels = if color.is_invalid() {
        None
    } else {
        apps::read_bitmap_bgra(color, 32).map(|(w, h, bgra)| {
            let mask_px = if mask.is_invalid() {
                None
            } else {
                apps::read_bitmap_bgra(mask, 32).filter(|(mw, mh, _)| *mw == w && *mh == h)
            };
            (w, h, icon_to_rgba(&bgra, mask_px.as_ref().map(|(_, _, px)| px.as_slice())))
        })
    };
    for bitmap in [color, mask] {
        if !bitmap.is_invalid() {
            // SAFETY: GetIconInfo created this bitmap for us; it is deleted once.
            let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        }
    }
    let (w, h, rgba) = pixels?;
    let png = apps::encode_png(w, h, &rgba)?;
    Some(format!("data:image/png;base64,{}", STANDARD.encode(png)))
}

// ---------------------------------------------------------------------------
// Commands.

/// The visible icons. Errors when the host is not running, so the dock can fall
/// back to the native tray.
#[tauri::command]
pub fn get_tray_icons() -> Result<Vec<IconView>, String> {
    let mut guard = store();
    let store = guard.as_mut().ok_or("the tray host is not running")?;
    store.retain_alive(is_alive);
    Ok(store.views())
}

/// Forwards a click on an icon to its owner, the way Explorer does.
#[tauri::command]
pub fn tray_click(id: String, button: String) -> Result<(), String> {
    let button = parse_button(&button)?;
    let icon = {
        let guard = store();
        let store = guard.as_ref().ok_or("the tray host is not running")?;
        store
            .icons
            .iter()
            .find(|icon| icon.id.to_string() == id)
            .cloned()
            .ok_or("unknown tray icon")?
    };
    let owner = hwnd_from_u32(icon.hwnd);
    if !is_alive(icon.hwnd) {
        let dropped = store().as_mut().is_some_and(|store| store.retain_alive(is_alive));
        if dropped {
            emit_changed();
        }
        return Err("the icon's app has exited".into());
    }
    if icon.callback == 0 {
        return Ok(());
    }
    let mut pid = 0u32;
    // SAFETY: owner is a live window and pid a valid out-pointer.
    unsafe { GetWindowThreadProcessId(owner, Some(&mut pid as *mut u32)) };
    if pid != 0 {
        // Lucent has the foreground (the user just clicked the panel), so it may
        // hand it on. The app needs it to show its menu or window in front.
        // SAFETY: plain call; failure only means the app may open behind.
        let _ = unsafe { AllowSetForegroundWindow(pid) };
    }
    let mut cursor = POINT::default();
    // SAFETY: cursor is a valid out-pointer.
    let _ = unsafe { GetCursorPos(&mut cursor) };
    for event in click_events(button, icon.version) {
        let (wp, lp) = callback_params(icon.version, icon.uid, event, cursor.x, cursor.y);
        // SAFETY: a post to the owner window with plain values.
        unsafe { PostMessageW(Some(owner), icon.callback, WPARAM(wp), LPARAM(lp)) }
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload as shell32 builds it, with the given fields.
    fn payload(command: u32, hwnd: u32, uid: u32, flags: u32, tip: &str) -> Vec<u8> {
        let mut bytes = vec![0u8; 964];
        let put = |bytes: &mut Vec<u8>, at: usize, v: u32| bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut bytes, OFF_SIGNATURE, TRAY_SIGNATURE);
        put(&mut bytes, OFF_COMMAND, command);
        put(&mut bytes, 8, 956);
        put(&mut bytes, OFF_HWND, hwnd);
        put(&mut bytes, OFF_UID, uid);
        put(&mut bytes, OFF_FLAGS, flags);
        put(&mut bytes, OFF_CALLBACK, 0x8001);
        put(&mut bytes, OFF_HICON, 0x1234);
        for (i, unit) in tip.encode_utf16().enumerate() {
            bytes[OFF_TIP + i * 2..OFF_TIP + i * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    fn msg(command: u32, flags: u32, tip: &str) -> TrayMessage {
        parse_tray_data(&payload(command, 0x10, 7, flags, tip)).expect("valid payload")
    }

    #[test]
    fn parses_an_add_payload() {
        let m = msg(NIM_ADD, NIF_MESSAGE | NIF_ICON | NIF_TIP, "Discord");
        assert_eq!(m.command, NIM_ADD);
        assert_eq!((m.hwnd, m.uid, m.callback, m.hicon), (0x10, 7, 0x8001, 0x1234));
        assert_eq!(m.tip, "Discord");
        assert_eq!(m.guid, None);
    }

    #[test]
    fn rejects_short_or_unsigned_payloads() {
        assert!(parse_tray_data(&[0u8; 16]).is_none());
        let mut bytes = payload(NIM_ADD, 1, 1, 0, "");
        bytes[0] = 0;
        assert!(parse_tray_data(&bytes).is_none());
    }

    #[test]
    fn short_payload_reads_missing_fields_as_zero() {
        let bytes = payload(NIM_ADD, 1, 1, NIF_TIP, "Steam");
        let m = parse_tray_data(&bytes[..300]).expect("long enough");
        assert_eq!(m.tip, "Steam");
        assert_eq!(m.version, 0);
        assert_eq!(m.state, 0);
    }

    #[test]
    fn tip_stops_at_nul_and_at_128_chars() {
        let long = "x".repeat(200);
        let m = msg(NIM_ADD, NIF_TIP, &long);
        assert_eq!(m.tip.len(), 128);
    }

    #[test]
    fn guid_is_used_only_with_nif_guid() {
        let mut bytes = payload(NIM_ADD, 1, 1, NIF_GUID, "");
        bytes[OFF_GUID] = 0xAB;
        assert!(parse_tray_data(&bytes).unwrap().guid.is_some());
        let mut bytes = payload(NIM_ADD, 1, 1, 0, "");
        bytes[OFF_GUID] = 0xAB;
        assert!(parse_tray_data(&bytes).unwrap().guid.is_none());
    }

    #[test]
    fn add_modify_delete_round_trip() {
        let mut store = Store::default();
        let added = store.apply(&msg(NIM_ADD, NIF_MESSAGE | NIF_ICON | NIF_TIP, "A"), Some("img1".into()));
        assert_eq!(added, Applied { ok: true, changed: true });
        assert_eq!(store.views()[0].tip, "A");

        let same = store.apply(&msg(NIM_MODIFY, NIF_TIP, "A"), None);
        assert_eq!(same, Applied { ok: true, changed: false });

        let modified = store.apply(&msg(NIM_MODIFY, NIF_ICON, ""), Some("img2".into()));
        assert!(modified.changed);
        let view = &store.views()[0];
        assert_eq!((view.tip.as_str(), view.image.as_deref()), ("A", Some("img2")));

        let deleted = store.apply(&msg(NIM_DELETE, 0, ""), None);
        assert_eq!(deleted, Applied { ok: true, changed: true });
        assert!(store.views().is_empty());
        assert!(!store.apply(&msg(NIM_DELETE, 0, ""), None).ok);
    }

    #[test]
    fn repeated_add_updates_in_place() {
        let mut store = Store::default();
        store.apply(&msg(NIM_ADD, NIF_TIP, "A"), None);
        store.apply(&msg(NIM_ADD, NIF_TIP, "B"), None);
        assert_eq!(store.icons.len(), 1);
        assert_eq!(store.icons[0].tip, "B");
    }

    #[test]
    fn modify_of_unknown_adds_only_with_an_icon() {
        let mut store = Store::default();
        assert!(!store.apply(&msg(NIM_MODIFY, NIF_TIP, "A"), None).ok);
        assert!(store.icons.is_empty());
        assert!(store.apply(&msg(NIM_MODIFY, NIF_ICON, ""), Some("img".into())).ok);
        assert_eq!(store.icons.len(), 1);
    }

    #[test]
    fn hidden_state_hides_and_shows() {
        let mut store = Store::default();
        store.apply(&msg(NIM_ADD, NIF_TIP, "A"), None);
        let mut hide = msg(NIM_MODIFY, NIF_STATE, "");
        hide.state = NIS_HIDDEN;
        hide.state_mask = NIS_HIDDEN;
        assert!(store.apply(&hide, None).changed);
        assert!(store.views().is_empty());
        hide.state = 0;
        store.apply(&hide, None);
        assert_eq!(store.views().len(), 1);
    }

    #[test]
    fn setversion_is_stored() {
        let mut store = Store::default();
        store.apply(&msg(NIM_ADD, 0, ""), None);
        let mut v = msg(NIM_SETVERSION, 0, "");
        v.version = 4;
        assert!(store.apply(&v, None).ok);
        assert_eq!(store.icons[0].version, 4);
    }

    #[test]
    fn guid_identity_ignores_hwnd_and_uid() {
        let mut store = Store::default();
        let mut add = msg(NIM_ADD, NIF_GUID | NIF_TIP, "G");
        add.guid = Some([1; 16]);
        store.apply(&add, None);
        let mut del = msg(NIM_DELETE, NIF_GUID, "");
        del.guid = Some([1; 16]);
        del.hwnd = 0;
        del.uid = 99;
        assert!(store.apply(&del, None).ok);
        assert!(store.icons.is_empty());
    }

    #[test]
    fn owner_names_fill_once() {
        let mut store = Store::default();
        store.apply(&msg(NIM_ADD, 0, ""), None);
        assert!(store.fill_names(|_| Some("Discord".into())));
        assert_eq!(store.views()[0].app, "Discord");
        assert!(!store.fill_names(|_| Some("Other".into())));
        assert_eq!(store.views()[0].app, "Discord");
    }

    #[test]
    fn dead_owners_are_dropped() {
        let mut store = Store::default();
        store.apply(&msg(NIM_ADD, 0, ""), None);
        assert!(!store.retain_alive(|_| true));
        assert!(store.retain_alive(|_| false));
        assert!(store.icons.is_empty());
    }

    #[test]
    fn click_events_follow_the_version() {
        assert_eq!(click_events(Button::Left, 0), vec![0x201, 0x202]);
        assert_eq!(click_events(Button::Left, 4), vec![0x201, 0x202, WM_USER]);
        assert_eq!(click_events(Button::Right, 3), vec![0x204, 0x205, 0x7B]);
        assert_eq!(click_events(Button::Right, 0), vec![0x204, 0x205]);
        assert_eq!(click_events(Button::Double, 4), vec![0x203, 0x202]);
    }

    #[test]
    fn callback_params_for_old_and_v4() {
        assert_eq!(callback_params(0, 7, 0x205, 100, 200), (7, 0x205));
        let (wp, lp) = callback_params(4, 7, 0x7B, 100, 200);
        assert_eq!(wp, (200 << 16) | 100);
        assert_eq!(lp, (7 << 16) | 0x7B);
        // Negative coordinates (a monitor left of the primary) keep 16 bits each.
        let (wp, _) = callback_params(4, 1, 0x201, -5, 10);
        assert_eq!(wp & 0xFFFF, 0xFFFB);
        assert_eq!(wp >> 16, 10);
    }

    #[test]
    fn unknown_deletes_after_our_broadcast_stay_with_lucent() {
        let soon = Some(Duration::from_secs(1));
        let late = Some(BROADCAST_GRACE + Duration::from_secs(1));
        assert!(!forward_delete(false, soon));
        assert!(forward_delete(true, soon));
        assert!(forward_delete(false, late));
        assert!(forward_delete(false, None));
    }

    #[test]
    fn buttons_are_validated() {
        assert_eq!(parse_button("right"), Ok(Button::Right));
        assert!(parse_button("middle").is_err());
    }

    #[test]
    fn icon_with_alpha_keeps_it_straight() {
        let bgra = [10, 20, 30, 128, 0, 0, 0, 0];
        assert_eq!(icon_to_rgba(&bgra, None), vec![30, 20, 10, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn icon_without_alpha_uses_the_mask() {
        let bgra = [10, 20, 30, 0, 40, 50, 60, 0];
        let mask = [0, 0, 0, 0, 255, 255, 255, 0];
        assert_eq!(icon_to_rgba(&bgra, Some(&mask)), vec![30, 20, 10, 255, 0, 0, 0, 0]);
        assert_eq!(icon_to_rgba(&bgra, None), vec![30, 20, 10, 255, 60, 50, 40, 255]);
    }
}

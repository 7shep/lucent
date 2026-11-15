//! Quick settings backend: volume, radios, network, brightness, power status,
//! settings shortcuts, and power actions. See docs/taskbar-spec.md, module `system`.
//!
//! `get_system` gathers each piece on its own thread with its own deadline. A
//! piece that fails or does not answer in time is None (or false) and never
//! fails the whole state.

use std::os::windows::process::CommandExt;
use std::sync::mpsc::{channel, sync_channel, Receiver};
use std::time::{Duration, Instant};

use windows::core::{Interface, RuntimeType, HSTRING, PCWSTR, w};
use windows::Devices::Radios::{Radio, RadioAccessStatus, RadioKind, RadioState};
use windows::Networking::Connectivity::{NetworkConnectivityLevel, NetworkInformation};
use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, SetMonitorBrightness, PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::{HANDLE, POINT};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SetSuspendState, SYSTEM_POWER_STATUS};
use windows::Win32::System::Shutdown::{ExitWindowsEx, LockWorkStation, EWX_LOGOFF, SHUTDOWN_REASON};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows_future::{AsyncOperationCompletedHandler, AsyncStatus, IAsyncInfo, IAsyncOperation};

use crate::model::{Battery, SystemState};

/// Deadline for each piece of `get_system`, measured from the start of the gather.
const PIECE_TIMEOUT: Duration = Duration::from_secs(2);
/// DDC/CI brightness is slow; it gets a tighter bound (the spec asks for ~1 s).
const DDC_TIMEOUT: Duration = Duration::from_secs(1);
/// Timeout for a single WinRT async call inside a piece.
const WINRT_WAIT: Duration = Duration::from_millis(1500);

/// BatteryFlag bits and values (GetSystemPowerStatus).
const BATTERY_FLAG_CHARGING: u8 = 8;
const BATTERY_FLAG_NO_BATTERY: u8 = 128;
const BATTERY_FLAG_UNKNOWN: u8 = 255;
/// ACLineStatus value for AC power.
const AC_ONLINE: u8 = 1;

/// Virtual-key code of the left Windows key.
const VK_WIN: u16 = 0x5B;

/// Power actions the `power` command accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Lock,
    Sleep,
    SignOut,
    Restart,
    Shutdown,
}

/// `lucent --print-system`: the current SystemState as JSON.
pub fn print_probe() {
    let state = gather();
    match serde_json::to_string_pretty(&state) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("system: cannot encode state: {e}"),
    }
}

#[tauri::command]
pub async fn get_system() -> SystemState {
    tauri::async_runtime::spawn_blocking(gather)
        .await
        .unwrap_or_default()
}

#[tauri::command]
pub async fn set_volume(value: f32) -> Result<(), String> {
    let value = clamp_unit(value).ok_or("volume is not a number")?;
    run_blocking(move || set_volume_blocking(value)).await
}

#[tauri::command]
pub async fn set_muted(muted: bool) -> Result<(), String> {
    run_blocking(move || set_muted_blocking(muted)).await
}

#[tauri::command]
pub async fn set_radio(kind: String, on: bool) -> Result<(), String> {
    let kind = radio_kind(&kind).ok_or_else(|| format!("unknown radio: {kind}"))?;
    run_blocking(move || set_radio_blocking(kind, on)).await
}

#[tauri::command]
pub async fn set_brightness(value: f32) -> Result<(), String> {
    let value = clamp_unit(value).ok_or("brightness is not a number")?;
    run_blocking(move || set_brightness_blocking(value)).await
}

#[tauri::command]
pub fn open_settings(page: String) -> Result<(), String> {
    let uri = settings_uri(&page).ok_or_else(|| format!("unknown settings page: {page}"))?;
    let target = HSTRING::from(uri);
    // SAFETY: every pointer passed is valid for the call. `target` is a live
    // HSTRING, the verb is a static literal, and the optional parameters and
    // directory are null.
    let code = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &target,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = code.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecute failed with code {code}"))
    }
}

#[tauri::command]
pub fn send_shortcut(name: String) -> Result<(), String> {
    let letter = shortcut_letter(&name).ok_or_else(|| format!("unknown shortcut: {name}"))?;
    let inputs = shortcut_inputs(letter);
    send_inputs(&inputs)
}

#[tauri::command]
pub fn power(action: String) -> Result<(), String> {
    let action = power_action(&action).ok_or_else(|| format!("unknown power action: {action}"))?;
    match action {
        PowerAction::Lock => lock_workstation(),
        PowerAction::Sleep => sleep(),
        PowerAction::SignOut => sign_out(),
        PowerAction::Restart => run_shutdown_exe("/r"),
        PowerAction::Shutdown => run_shutdown_exe("/s"),
    }
}

// ---------------------------------------------------------------------------
// Whitelists and pure helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Settings page name to its `ms-settings:` URI. Anything else is None.
fn settings_uri(page: &str) -> Option<&'static str> {
    Some(match page {
        "nightlight" => "ms-settings:nightlight",
        "focus" => "ms-settings:focus",
        "network" => "ms-settings:network",
        "wifi" => "ms-settings:network-wifi",
        "bluetooth" => "ms-settings:bluetooth",
        "sound" => "ms-settings:sound",
        "display" => "ms-settings:display",
        "power" => "ms-settings:powersleep",
        "airplane" => "ms-settings:network-airplanemode",
        "settings" => "ms-settings:",
        "accounts" => "ms-settings:yourinfo",
        _ => return None,
    })
}

/// Shortcut name to the letter key pressed with Win. Anything else is None.
fn shortcut_letter(name: &str) -> Option<u16> {
    match name {
        "notifications" => Some(u16::from(b'N')),
        "quick_settings" => Some(u16::from(b'A')),
        "cast" => Some(u16::from(b'K')),
        _ => None,
    }
}

/// The four key events of a Win+letter shortcut: (virtual key, is key-up).
fn shortcut_steps(letter: u16) -> [(u16, bool); 4] {
    [(VK_WIN, false), (letter, false), (letter, true), (VK_WIN, true)]
}

fn shortcut_inputs(letter: u16) -> [INPUT; 4] {
    shortcut_steps(letter).map(|(vk, up)| key_input(vk, up))
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Power action name to its action. Anything else is None.
fn power_action(action: &str) -> Option<PowerAction> {
    match action {
        "lock" => Some(PowerAction::Lock),
        "sleep" => Some(PowerAction::Sleep),
        "signout" => Some(PowerAction::SignOut),
        "restart" => Some(PowerAction::Restart),
        "shutdown" => Some(PowerAction::Shutdown),
        _ => None,
    }
}

/// Radio name to its kind. Only Wi-Fi and Bluetooth are controllable.
fn radio_kind(kind: &str) -> Option<RadioKind> {
    match kind {
        "wifi" => Some(RadioKind::WiFi),
        "bluetooth" => Some(RadioKind::Bluetooth),
        _ => None,
    }
}

/// State of the radio of `kind` in a list of (kind, is on). None when absent.
fn radio_flag(list: &[(RadioKind, bool)], kind: RadioKind) -> Option<bool> {
    list.iter().find(|(k, _)| *k == kind).map(|(_, on)| *on)
}

/// Clamps a 0..=1 input. NaN is rejected (None).
fn clamp_unit(value: f32) -> Option<f32> {
    if value.is_nan() {
        None
    } else {
        Some(value.clamp(0.0, 1.0))
    }
}

/// Maps a DDC/CI reading to 0..=1 across the monitor's min..max range.
/// A degenerate range (max <= min) is unsupported (None).
fn ddc_fraction(min: u32, cur: u32, max: u32) -> Option<f32> {
    if max <= min {
        return None;
    }
    let span = (max - min) as f32;
    Some((cur.saturating_sub(min) as f32 / span).clamp(0.0, 1.0))
}

/// Maps a 0..=1 fraction onto the monitor's min..max DDC/CI range.
fn ddc_value(min: u32, max: u32, fraction: f32) -> Option<u32> {
    if max <= min {
        return None;
    }
    let fraction = clamp_unit(fraction)?;
    let span = (max - min) as f64;
    let raw = min as f64 + (fraction as f64 * span).round();
    Some(raw.min(max as f64) as u32)
}

/// Battery and AC state from GetSystemPowerStatus fields.
/// `battery` is None when the flag says no battery (128) or unknown (255).
/// `plugged_in` is true when ACLineStatus is online (1).
fn battery_state(flag: u8, percent: u8, ac_line: u8) -> (Option<Battery>, bool) {
    let plugged_in = ac_line == AC_ONLINE;
    // 255 percent means Windows does not know the charge level.
    if flag & BATTERY_FLAG_NO_BATTERY != 0 || flag == BATTERY_FLAG_UNKNOWN || percent > 100 {
        return (None, plugged_in);
    }
    let battery = Battery {
        percent,
        charging: flag & BATTERY_FLAG_CHARGING != 0,
    };
    (Some(battery), plugged_in)
}

/// True when the connectivity level means the profile is really connected.
fn connected_level(level: NetworkConnectivityLevel) -> bool {
    level == NetworkConnectivityLevel::InternetAccess
        || level == NetworkConnectivityLevel::ConstrainedInternetAccess
}

// ---------------------------------------------------------------------------
// Gathering
// ---------------------------------------------------------------------------

fn gather() -> SystemState {
    let start = Instant::now();
    let audio = spawn_piece(read_audio);
    let radios = spawn_piece(read_radios);
    let network = spawn_piece(read_network);
    let brightness = spawn_piece(read_brightness);
    let power = spawn_piece(read_power);

    let (volume, muted) = collect(audio, start + PIECE_TIMEOUT).unwrap_or_default();
    let (wifi, bluetooth) = collect(radios, start + PIECE_TIMEOUT).unwrap_or_default();
    let (network_name, ethernet) = collect(network, start + PIECE_TIMEOUT).unwrap_or_default();
    let brightness = collect(brightness, start + DDC_TIMEOUT).unwrap_or_default();
    let (battery, plugged_in) = collect(power, start + PIECE_TIMEOUT).unwrap_or_default();

    SystemState {
        volume,
        muted,
        wifi,
        network_name,
        ethernet,
        bluetooth,
        brightness,
        battery,
        plugged_in,
    }
}

/// Runs `f` on its own thread. The receiver yields its result, or nothing if
/// the thread could not start or panicked.
fn spawn_piece<T, F>(f: F) -> Receiver<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = channel();
    // A failed spawn drops `tx`, so the receiver reports nothing.
    let _ = std::thread::Builder::new()
        .name("lucent-system".into())
        .spawn(move || {
            let _ = tx.send(f());
        });
    rx
}

/// Waits for a piece until `deadline`. A piece still running then is abandoned.
fn collect<T>(rx: Receiver<T>, deadline: Instant) -> Option<T> {
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
}

/// Runs blocking work (COM, DDC) off the async runtime and flattens the join error.
async fn run_blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(work).await {
        Ok(result) => result,
        Err(e) => Err(format!("worker failed: {e}")),
    }
}

/// Initialises COM (MTA) and WinRT on the current thread and balances only the
/// calls that succeeded, on drop, including early returns.
struct Apartment {
    co: bool,
    ro: bool,
}

impl Apartment {
    fn enter() -> Self {
        // SAFETY: called on the current thread with a valid apartment flag and
        // no reserved pointer. A successful call is balanced in Drop.
        let co = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        // SAFETY: called on the current thread with a valid apartment flag.
        // A successful call is balanced in Drop.
        let ro = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok();
        Self { co, ro }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.ro {
            // SAFETY: balances the successful RoInitialize on this same thread.
            unsafe { RoUninitialize() };
        }
        if self.co {
            // SAFETY: balances the successful CoInitializeEx on this same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// Waits for a WinRT async operation for at most `timeout`. Timeouts cancel
/// the operation. Failed or cancelled operations give None. (Unlike `join()`,
/// this cannot hang the caller.)
fn wait_op<T: RuntimeType + 'static>(op: &IAsyncOperation<T>, timeout: Duration) -> Option<T> {
    if op.Status().ok()? != AsyncStatus::Started {
        return op.GetResults().ok();
    }
    let (tx, rx) = sync_channel::<()>(1);
    let handler = AsyncOperationCompletedHandler::new(move |_op, _status| {
        let _ = tx.try_send(());
        Ok(())
    });
    op.SetCompleted(&handler).ok()?;
    if rx.recv_timeout(timeout).is_err() {
        if let Ok(info) = op.cast::<IAsyncInfo>() {
            let _ = info.Cancel();
        }
        return None;
    }
    op.GetResults().ok()
}

// ---------------------------------------------------------------------------
// Volume and mute (IAudioEndpointVolume on the default render device)
// ---------------------------------------------------------------------------

fn default_endpoint_volume() -> Option<IAudioEndpointVolume> {
    // SAFETY: creates the in-process device enumerator from its CLSID with no
    // outer aggregate. Runs on a thread where COM was initialised by Apartment.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok()?;
    // SAFETY: plain call on a live enumerator; no caller-owned memory is referenced.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.ok()?;
    // SAFETY: activates the endpoint volume interface on a live device with no
    // activation parameters.
    unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }.ok()
}

fn read_audio() -> (Option<f32>, Option<bool>) {
    let _apt = Apartment::enter();
    let Some(endpoint) = default_endpoint_volume() else {
        return (None, None);
    };
    // SAFETY: `endpoint` is a live interface on this thread; the calls take no pointers.
    let volume = unsafe { endpoint.GetMasterVolumeLevelScalar() }
        .ok()
        .and_then(clamp_unit);
    // SAFETY: as above.
    let muted = unsafe { endpoint.GetMute() }.ok().map(|m| m.as_bool());
    (volume, muted)
}

fn set_volume_blocking(value: f32) -> Result<(), String> {
    let _apt = Apartment::enter();
    let endpoint = default_endpoint_volume().ok_or("no default output device")?;
    // SAFETY: `endpoint` is a live interface on this thread. A null event
    // context means no change-notification GUID is sent.
    unsafe { endpoint.SetMasterVolumeLevelScalar(value, std::ptr::null()) }
        .map_err(|e| e.to_string())
}

fn set_muted_blocking(muted: bool) -> Result<(), String> {
    let _apt = Apartment::enter();
    let endpoint = default_endpoint_volume().ok_or("no default output device")?;
    // SAFETY: `endpoint` is a live interface on this thread. A null event
    // context means no change-notification GUID is sent.
    unsafe { endpoint.SetMute(muted, std::ptr::null()) }.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Radios (Wi-Fi, Bluetooth)
// ---------------------------------------------------------------------------

/// (kind, is on) for every radio the system reports. Radios that fail to read are skipped.
fn radio_list() -> Option<Vec<(RadioKind, bool)>> {
    let op = Radio::GetRadiosAsync().ok()?;
    let radios = wait_op(&op, WINRT_WAIT)?;
    let count = radios.Size().ok()?;
    let mut out = Vec::new();
    for i in 0..count {
        let Ok(radio) = radios.GetAt(i) else {
            continue;
        };
        if let (Ok(kind), Ok(state)) = (radio.Kind(), radio.State()) {
            out.push((kind, state == RadioState::On));
        }
    }
    Some(out)
}

fn read_radios() -> (Option<bool>, Option<bool>) {
    let _apt = Apartment::enter();
    let Some(list) = radio_list() else {
        return (None, None);
    };
    (
        radio_flag(&list, RadioKind::WiFi),
        radio_flag(&list, RadioKind::Bluetooth),
    )
}

fn set_radio_blocking(kind: RadioKind, on: bool) -> Result<(), String> {
    let _apt = Apartment::enter();
    let access_op = Radio::RequestAccessAsync().map_err(|e| e.to_string())?;
    let access = wait_op(&access_op, WINRT_WAIT).ok_or("radio access request did not complete")?;
    if access != RadioAccessStatus::Allowed {
        return Err("radio access is not allowed".into());
    }

    let op = Radio::GetRadiosAsync().map_err(|e| e.to_string())?;
    let radios = wait_op(&op, WINRT_WAIT).ok_or("could not list radios")?;
    let count = radios.Size().map_err(|e| e.to_string())?;
    for i in 0..count {
        let Ok(radio) = radios.GetAt(i) else {
            continue;
        };
        if radio.Kind().ok() != Some(kind) {
            continue;
        }
        let state = if on { RadioState::On } else { RadioState::Off };
        let op = radio.SetStateAsync(state).map_err(|e| e.to_string())?;
        let status = wait_op(&op, WINRT_WAIT).ok_or("radio state change did not complete")?;
        return if status == RadioAccessStatus::Allowed {
            Ok(())
        } else {
            Err("radio state change was refused".into())
        };
    }
    Err("this machine has no such radio".into())
}

// ---------------------------------------------------------------------------
// Network name and ethernet flag
// ---------------------------------------------------------------------------

fn read_network() -> (Option<String>, bool) {
    let _apt = Apartment::enter();
    let Ok(profile) = NetworkInformation::GetInternetConnectionProfile() else {
        return (None, false);
    };
    let connected = profile
        .GetNetworkConnectivityLevel()
        .map(connected_level)
        .unwrap_or(false);
    if !connected {
        return (None, false);
    }
    let name = profile
        .ProfileName()
        .ok()
        .map(|n| n.to_string())
        .filter(|n| !n.trim().is_empty());
    // A failed wireless check counts as wireless, so the ethernet flag stays false.
    let wireless = profile.IsWlanConnectionProfile().unwrap_or(true);
    (name, !wireless)
}

// ---------------------------------------------------------------------------
// Brightness (DDC/CI on the primary monitor)
// ---------------------------------------------------------------------------

/// Runs `f` with the first physical monitor of the primary display. The
/// monitors are always destroyed afterwards, on every path.
fn with_primary_monitor<T>(f: impl FnOnce(HANDLE) -> Option<T>) -> Option<T> {
    // SAFETY: takes its arguments by value and reads no caller memory.
    let hmonitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    if hmonitor.0.is_null() {
        return None;
    }
    let mut count = 0u32;
    // SAFETY: `count` is a valid, writable u32 for the duration of the call.
    unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count) }.ok()?;
    if count == 0 {
        return None;
    }
    let mut monitors: Vec<PHYSICAL_MONITOR> =
        (0..count).map(|_| PHYSICAL_MONITOR::default()).collect();
    // SAFETY: `monitors` holds exactly `count` writable elements.
    let fetched = unsafe { GetPhysicalMonitorsFromHMONITOR(hmonitor, &mut monitors) }.is_ok();
    let result = if fetched {
        monitors.first().and_then(|m| f(m.hPhysicalMonitor))
    } else {
        None
    };
    if fetched {
        // SAFETY: `monitors` came from GetPhysicalMonitorsFromHMONITOR. It is
        // destroyed exactly once, after `f` has finished with the handles.
        let _ = unsafe { DestroyPhysicalMonitors(&monitors) };
    }
    result
}

fn monitor_range(handle: HANDLE) -> Option<(u32, u32, u32)> {
    let (mut min, mut cur, mut max) = (0u32, 0u32, 0u32);
    // SAFETY: `handle` is a live physical monitor handle, and the three
    // out-pointers reference valid locals.
    let ok = unsafe { GetMonitorBrightness(handle, &mut min, &mut cur, &mut max) } != 0;
    ok.then_some((min, cur, max))
}

fn read_brightness() -> Option<f32> {
    with_primary_monitor(|handle| {
        let (min, cur, max) = monitor_range(handle)?;
        ddc_fraction(min, cur, max)
    })
}

fn set_brightness_blocking(fraction: f32) -> Result<(), String> {
    with_primary_monitor(|handle| {
        let (min, _, max) = monitor_range(handle)?;
        let raw = ddc_value(min, max, fraction)?;
        // SAFETY: `handle` is a live physical monitor handle. It is destroyed
        // by `with_primary_monitor` after this closure returns.
        let ok = unsafe { SetMonitorBrightness(handle, raw) } != 0;
        ok.then_some(())
    })
    .ok_or_else(|| "brightness is not supported on this display".to_string())
}

// ---------------------------------------------------------------------------
// Battery and AC state
// ---------------------------------------------------------------------------

fn read_power() -> (Option<Battery>, bool) {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a valid, writable SYSTEM_POWER_STATUS for the duration of the call.
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
        return (None, false);
    }
    battery_state(
        status.BatteryFlag,
        status.BatteryLifePercent,
        status.ACLineStatus,
    )
}

// ---------------------------------------------------------------------------
// Input and power actions
// ---------------------------------------------------------------------------

fn send_inputs(inputs: &[INPUT]) -> Result<(), String> {
    let size = std::mem::size_of::<INPUT>() as i32;
    // SAFETY: `inputs` is a live slice of fully initialised INPUT values, and
    // `size` is size_of::<INPUT>().
    let sent = unsafe { SendInput(inputs, size) } as usize;
    if sent == inputs.len() {
        Ok(())
    } else {
        Err(format!("SendInput accepted {sent} of {} events", inputs.len()))
    }
}

fn lock_workstation() -> Result<(), String> {
    // SAFETY: takes no arguments.
    unsafe { LockWorkStation() }.map_err(|e| e.to_string())
}

fn sleep() -> Result<(), String> {
    // SAFETY: takes only plain booleans and no pointers.
    if unsafe { SetSuspendState(false, false, false) } {
        Ok(())
    } else {
        Err("sleep was refused".into())
    }
}

fn sign_out() -> Result<(), String> {
    // SAFETY: takes only plain flag values and no pointers.
    unsafe { ExitWindowsEx(EWX_LOGOFF, SHUTDOWN_REASON(0)) }.map_err(|e| e.to_string())
}

fn run_shutdown_exe(switch: &str) -> Result<(), String> {
    std::process::Command::new("shutdown.exe")
        .args([switch, "/t", "0"])
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_pages_map_to_uris() {
        assert_eq!(settings_uri("nightlight"), Some("ms-settings:nightlight"));
        assert_eq!(settings_uri("focus"), Some("ms-settings:focus"));
        assert_eq!(settings_uri("network"), Some("ms-settings:network"));
        assert_eq!(settings_uri("wifi"), Some("ms-settings:network-wifi"));
        assert_eq!(settings_uri("bluetooth"), Some("ms-settings:bluetooth"));
        assert_eq!(settings_uri("sound"), Some("ms-settings:sound"));
        assert_eq!(settings_uri("display"), Some("ms-settings:display"));
        assert_eq!(settings_uri("power"), Some("ms-settings:powersleep"));
        assert_eq!(
            settings_uri("airplane"),
            Some("ms-settings:network-airplanemode")
        );
        assert_eq!(settings_uri("settings"), Some("ms-settings:"));
        assert_eq!(settings_uri("accounts"), Some("ms-settings:yourinfo"));
    }

    #[test]
    fn settings_rejects_unknown_pages() {
        for page in ["", "Settings", "ms-settings:", "display ", "../../evil", "shell:startup"] {
            assert_eq!(settings_uri(page), None, "accepted {page:?}");
        }
    }

    #[test]
    fn shortcut_whitelist_and_rejection() {
        assert_eq!(shortcut_letter("notifications"), Some(u16::from(b'N')));
        assert_eq!(shortcut_letter("quick_settings"), Some(u16::from(b'A')));
        assert_eq!(shortcut_letter("cast"), Some(u16::from(b'K')));
        for name in ["", "Notifications", "win+n", "lock", "cast ", "r"] {
            assert_eq!(shortcut_letter(name), None, "accepted {name:?}");
        }
    }

    #[test]
    fn shortcut_sends_win_down_letter_down_letter_up_win_up() {
        assert_eq!(
            shortcut_steps(u16::from(b'N')),
            [(0x5B, false), (0x4E, false), (0x4E, true), (0x5B, true)]
        );
    }

    #[test]
    fn power_whitelist_and_rejection() {
        assert_eq!(power_action("lock"), Some(PowerAction::Lock));
        assert_eq!(power_action("sleep"), Some(PowerAction::Sleep));
        assert_eq!(power_action("signout"), Some(PowerAction::SignOut));
        assert_eq!(power_action("restart"), Some(PowerAction::Restart));
        assert_eq!(power_action("shutdown"), Some(PowerAction::Shutdown));
        for action in ["", "LOCK", "hibernate", "poweroff", "shutdown /s", "logoff"] {
            assert_eq!(power_action(action), None, "accepted {action:?}");
        }
    }

    #[test]
    fn radio_whitelist_and_rejection() {
        assert_eq!(radio_kind("wifi"), Some(RadioKind::WiFi));
        assert_eq!(radio_kind("bluetooth"), Some(RadioKind::Bluetooth));
        for kind in ["", "WiFi", "airplane", "cellular", "wifi "] {
            assert_eq!(radio_kind(kind), None, "accepted {kind:?}");
        }
    }

    #[test]
    fn radio_flag_picks_by_kind_and_is_none_when_absent() {
        let list = [(RadioKind::Bluetooth, false), (RadioKind::WiFi, true)];
        assert_eq!(radio_flag(&list, RadioKind::WiFi), Some(true));
        assert_eq!(radio_flag(&list, RadioKind::Bluetooth), Some(false));
        assert_eq!(radio_flag(&[], RadioKind::WiFi), None);
    }

    #[test]
    fn clamp_unit_clamps_and_rejects_nan() {
        assert_eq!(clamp_unit(0.25), Some(0.25));
        assert_eq!(clamp_unit(-3.0), Some(0.0));
        assert_eq!(clamp_unit(7.0), Some(1.0));
        assert_eq!(clamp_unit(f32::INFINITY), Some(1.0));
        assert_eq!(clamp_unit(f32::NEG_INFINITY), Some(0.0));
        assert_eq!(clamp_unit(f32::NAN), None);
    }

    #[test]
    fn ddc_fraction_maps_range_to_unit() {
        assert_eq!(ddc_fraction(0, 50, 100), Some(0.5));
        assert_eq!(ddc_fraction(10, 10, 20), Some(0.0));
        assert_eq!(ddc_fraction(10, 20, 20), Some(1.0));
        // Readings outside the range are clamped.
        assert_eq!(ddc_fraction(10, 0, 20), Some(0.0));
        assert_eq!(ddc_fraction(10, 99, 20), Some(1.0));
        // A degenerate range is unsupported.
        assert_eq!(ddc_fraction(5, 5, 5), None);
        assert_eq!(ddc_fraction(9, 5, 3), None);
    }

    #[test]
    fn ddc_value_maps_unit_to_range_and_clamps_input() {
        assert_eq!(ddc_value(0, 100, 0.5), Some(50));
        assert_eq!(ddc_value(10, 20, 0.0), Some(10));
        assert_eq!(ddc_value(10, 20, 1.0), Some(20));
        assert_eq!(ddc_value(0, 100, 2.0), Some(100));
        assert_eq!(ddc_value(0, 100, -1.0), Some(0));
        assert_eq!(ddc_value(0, 100, f32::NAN), None);
        assert_eq!(ddc_value(5, 5, 0.5), None);
    }

    #[test]
    fn battery_flags_no_battery_and_unknown_give_none() {
        assert_eq!(battery_state(128, 0, 1), (None, true));
        assert_eq!(battery_state(255, 255, 0), (None, false));
    }

    #[test]
    fn battery_flags_charging_percent_and_ac() {
        assert_eq!(
            battery_state(8, 50, 0),
            (
                Some(Battery {
                    percent: 50,
                    charging: true
                }),
                false
            )
        );
        assert_eq!(
            battery_state(1, 80, 1),
            (
                Some(Battery {
                    percent: 80,
                    charging: false
                }),
                true
            )
        );
        // ACLineStatus 255 (unknown) is not plugged in.
        assert_eq!(battery_state(1, 80, 255).1, false);
        // An unknown percent (255) means no usable battery reading.
        assert_eq!(battery_state(1, 255, 1).0, None);
    }

    #[test]
    fn connectivity_levels() {
        assert!(connected_level(NetworkConnectivityLevel::InternetAccess));
        assert!(connected_level(NetworkConnectivityLevel::ConstrainedInternetAccess));
        assert!(!connected_level(NetworkConnectivityLevel::LocalAccess));
        assert!(!connected_level(NetworkConnectivityLevel::None));
    }
}

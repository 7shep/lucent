//! Data shapes shared by the backend modules and sent to the UI (JSON, camelCase).
//! This is the contract between Rust and ui/*.js. Change both sides together.

use serde::{Deserialize, Serialize};

/// Something the user can launch: a taskbar pin, a Start pin, or an app from
/// the Start "All apps" list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct App {
    /// Stable identity used to match windows to apps. The AppUserModelID when
    /// the app has one, otherwise the lowercase full path of the target exe.
    pub id: String,
    /// Display name ("Google Chrome").
    pub name: String,
    /// Shell parsing name used to launch it and to fetch its icon: a file path
    /// (.lnk or .exe) or `shell:AppsFolder\<AUMID>`.
    pub launch: String,
}

/// An entry of the Start menu's "All apps" list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CatalogApp {
    #[serde(flatten)]
    pub app: App,
    /// One of `apps::CATEGORIES`.
    pub category: String,
}

/// A file shown under Recent or in search results.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    /// Absolute path of the file itself (shortcuts already resolved).
    pub path: String,
    /// Last modified / last opened, epoch milliseconds.
    pub modified_ms: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    pub apps: Vec<App>,
    pub files: Vec<FileEntry>,
}

/// The signed-in user, for the Start menu footer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    pub name: String,
    /// PNG/JPEG data URL of the account picture, if one was found.
    pub avatar: Option<String>,
}

/// One top-level window that belongs on the taskbar.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DockWindow {
    /// HWND as an integer.
    pub hwnd: i64,
    pub title: String,
}

/// One icon on the dock: a pin, a running app, or both.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DockItem {
    pub id: String,
    pub name: String,
    /// Parsing name for launching and for `get_icon`.
    pub launch: String,
    pub pinned: bool,
    /// Open windows of this app, in z-order (topmost first). Empty when not running.
    pub windows: Vec<DockWindow>,
    /// One of its windows is the foreground window.
    pub active: bool,
    /// One of its windows is flashing for attention.
    pub flashing: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Battery {
    pub percent: u8,
    pub charging: bool,
}

/// Quick settings state. `None` means unavailable on this machine (the UI hides
/// the control).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SystemState {
    /// Master volume of the default output device, 0.0..=1.0.
    pub volume: Option<f32>,
    pub muted: Option<bool>,
    pub wifi: Option<bool>,
    /// Name of the connected network (Wi-Fi SSID or Ethernet profile name).
    pub network_name: Option<String>,
    /// True when the current internet connection is wired.
    pub ethernet: bool,
    pub bluetooth: Option<bool>,
    /// Brightness of the primary display, 0.0..=1.0.
    pub brightness: Option<f32>,
    /// None on desktops without a battery.
    pub battery: Option<Battery>,
    pub plugged_in: bool,
}

/// A rounded rectangle in logical pixels, relative to the window's top-left.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Corner radius.
    pub r: f64,
}

/// What sits behind the glass: the desktop wallpaper, blurred by the UI.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Backdrop {
    /// PNG data URL of the wallpaper, downscaled so its longer side is at most
    /// 640 px. None for a solid-colour background.
    pub image: Option<String>,
    /// How Windows lays the wallpaper out: "fill", "fit", "stretch", "tile",
    /// "center" or "span".
    pub style: String,
    /// Desktop background colour, "#rrggbb" (shows where the image does not).
    pub color: String,
    /// Primary monitor size in logical px.
    pub monitor_width: f64,
    pub monitor_height: f64,
}

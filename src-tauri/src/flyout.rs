//! The flyout window that hosts the Start, search, quick settings and
//! notifications panels, one at a time. See docs/taskbar-spec.md, module `flyout`.

use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

use crate::glass;

pub const LABEL: &str = "flyout";
const DOCK_LABEL: &str = "dock";

/// Transparent margin (logical px) around a panel, so the spring overshoot is not clipped.
const PAD: f64 = 24.0;
/// Distance (logical px) from the monitor's bottom edge to the panel's bottom edge.
const PANEL_BOTTOM_GAP: f64 = 66.0;
/// Distance (logical px) from the panel's bottom edge to the bar's centre: 12 gap + 22 half bar.
const BAR_CENTRE_BELOW_PANEL: f64 = 12.0 + 22.0;

/// The panels the flyout can host. Anything else is rejected.
pub const PANELS: [&str; 4] = ["start", "search", "quick_settings", "notifications"];

/// The panel currently open, if any. A static so main.rs needs no change.
static OPEN_PANEL: Mutex<Option<String>> = Mutex::new(None);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ShowPayload {
    panel: String,
    origin_x: f64,
    origin_y: f64,
    /// Flyout window's top-left, logical px relative to the primary monitor's top-left.
    window_x: f64,
    window_y: f64,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StatePayload {
    panel: Option<String>,
}

/// Returns the panel name as a static string when it is one of `PANELS`.
pub fn parse_panel(name: &str) -> Result<&'static str, String> {
    PANELS
        .iter()
        .copied()
        .find(|p| *p == name)
        .ok_or_else(|| format!("unknown panel `{name}`"))
}

/// Rejects non-finite values, and negative sizes.
fn check_geometry(x: f64, width: f64, height: f64, anchor_x: f64) -> Result<(), String> {
    if ![x, width, height, anchor_x].iter().all(|v| v.is_finite()) {
        return Err("flyout geometry must be finite".into());
    }
    if width < 0.0 || height < 0.0 {
        return Err("flyout size must not be negative".into());
    }
    Ok(())
}

/// Where the flyout window goes. The physical rect is what the OS places. The window_x and
/// window_y are the same top-left in logical px, relative to the primary monitor's top-left,
/// taken from the physical position so they match what is on screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Physical rect of the window.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Origin y of the spring, logical px from the window's top. The centre of the bar.
    pub origin_y: f64,
    /// Window top-left, logical px relative to the primary monitor's top-left.
    pub window_x: f64,
    pub window_y: f64,
}

/// Places the flyout window for a panel at logical `x` with the given size, on a monitor at
/// physical (`monitor_x`, `monitor_y`) of physical size `monitor_w` x `monitor_h` with DPI
/// `scale`. `x` is relative to the monitor's left edge. The window is the panel plus PAD on
/// every side, and the panel's bottom sits PANEL_BOTTOM_GAP above the monitor's bottom edge.
/// `monitor_w` is accepted for symmetry with the caller; horizontal placement only needs `x`.
#[allow(clippy::too_many_arguments)]
pub fn flyout_rect(
    monitor_x: i32,
    monitor_y: i32,
    _monitor_w: u32,
    monitor_h: u32,
    scale: f64,
    x: f64,
    width: f64,
    height: f64,
) -> Placement {
    let phys_w = ((width + 2.0 * PAD) * scale).round() as u32;
    let phys_h = ((height + 2.0 * PAD) * scale).round() as u32;
    let phys_x = monitor_x + ((x - PAD) * scale).round() as i32;
    // Window bottom = monitor bottom - (66 - 24): the panel bottom is 66 up, the window adds PAD below it.
    let window_bottom = monitor_y + monitor_h as i32 - ((PANEL_BOTTOM_GAP - PAD) * scale).round() as i32;
    let phys_y = window_bottom - phys_h as i32;
    let origin_y = PAD + height + BAR_CENTRE_BELOW_PANEL;
    Placement {
        x: phys_x,
        y: phys_y,
        width: phys_w,
        height: phys_h,
        origin_y,
        window_x: f64::from(phys_x - monitor_x) / scale,
        window_y: f64::from(phys_y - monitor_y) / scale,
    }
}

/// Horizontal origin of the spring, in the flyout window's logical px: the clicked icon's
/// centre relative to the window's left edge.
pub fn origin_x(anchor_x: f64, x: f64) -> f64 {
    anchor_x - x + PAD
}

fn lock_open() -> MutexGuard<'static, Option<String>> {
    OPEN_PANEL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window(LABEL)
        .ok_or_else(|| "flyout window not found".to_string())
}

fn publish_dock_state(app: &AppHandle, panel: Option<String>) -> Result<(), String> {
    app.emit_to(DOCK_LABEL, "flyout-state", StatePayload { panel })
        .map_err(|e| e.to_string())
}

/// Asks the flyout UI to close. No-op when no panel is open.
fn request_close(app: &AppHandle) -> Result<(), String> {
    let open = lock_open().is_some();
    if !open {
        return Ok(());
    }
    app.emit_to(LABEL, "flyout-close", ())
        .map_err(|e| e.to_string())
}

/// Hides the flyout and clears the open panel, then tells the dock.
fn hide_and_clear(app: &AppHandle) -> Result<(), String> {
    *lock_open() = None;
    let hidden = window(app).and_then(|w| w.hide().map_err(|e| e.to_string()));
    let published = publish_dock_state(app, None);
    hidden.and(published)
}

/// Called when the flyout window loses focus: asks the flyout UI to close.
pub fn on_focus_lost(app: &AppHandle) {
    if let Err(err) = request_close(app) {
        eprintln!("lucent: flyout close request failed: {err}");
    }
}

/// Hides the flyout at once (fullscreen app, explorer restart).
pub fn force_hide(app: &AppHandle) {
    if let Err(err) = hide_and_clear(app) {
        eprintln!("lucent: flyout hide failed: {err}");
    }
}

#[tauri::command]
pub fn flyout_open(app: AppHandle, panel: String, x: f64, width: f64, height: f64, anchor_x: f64) -> Result<(), String> {
    let panel = parse_panel(&panel)?;
    check_geometry(x, width, height, anchor_x)?;
    let window = window(&app)?;
    let monitor = window
        .primary_monitor()
        .map_err(|e| e.to_string())?
        .ok_or("no primary monitor")?;
    let position = monitor.position();
    let size = monitor.size();
    let placement = flyout_rect(
        position.x,
        position.y,
        size.width,
        size.height,
        monitor.scale_factor(),
        x,
        width,
        height,
    );
    let was_open = lock_open().is_some();

    window
        .set_size(PhysicalSize::new(placement.width, placement.height))
        .map_err(|e| e.to_string())?;
    window
        .set_position(PhysicalPosition::new(placement.x, placement.y))
        .map_err(|e| e.to_string())?;
    if !was_open {
        // Empty until the UI reports the panel's real rects, so nothing shows as a box.
        glass::set_region_rects(&window, &[])?;
        window.show().map_err(|e| e.to_string())?;
        // Shown either way; without focus it just won't close on click-away.
        if let Err(err) = window.set_focus() {
            eprintln!("lucent: flyout focus failed: {err}");
        }
    }
    *lock_open() = Some(panel.to_string());

    app.emit_to(
        LABEL,
        "flyout-show",
        ShowPayload {
            panel: panel.to_string(),
            origin_x: origin_x(anchor_x, x),
            origin_y: placement.origin_y,
            window_x: placement.window_x,
            window_y: placement.window_y,
        },
    )
    .map_err(|e| e.to_string())?;
    publish_dock_state(&app, Some(panel.to_string()))
}

#[tauri::command]
pub fn flyout_request_close(app: AppHandle) -> Result<(), String> {
    request_close(&app)
}

#[tauri::command]
pub fn flyout_hidden(app: AppHandle) -> Result<(), String> {
    hide_and_clear(&app)
}

#[cfg(test)]
mod tests {
    use super::{check_geometry, flyout_rect, origin_x, parse_panel, Placement};

    #[test]
    fn flyout_rect_at_100_percent_puts_panel_bottom_66_above_monitor_bottom() {
        // Start panel 440x600, centred on a 1920-wide monitor: left edge 740.
        let r = flyout_rect(0, 0, 1920, 1080, 1.0, 740.0, 440.0, 600.0);
        assert_eq!(
            r,
            Placement {
                x: 716,
                y: 390,
                width: 488,
                height: 648,
                origin_y: 658.0,
                window_x: 716.0,
                window_y: 390.0,
            }
        );
        // Panel bottom in physical px: 390 + 24 + 600 = 1014 = 1080 - 66.
        assert_eq!(r.y + 24 + 600, 1080 - 66);
    }

    #[test]
    fn flyout_rect_at_150_percent_scales_window_and_gap() {
        let r = flyout_rect(0, 0, 2880, 1620, 1.5, 740.0, 440.0, 600.0);
        assert_eq!((r.x, r.y, r.width, r.height, r.origin_y), (1074, 585, 732, 972, 658.0));
        // Panel bottom: 585 + 36 + 900 = 1521 = 1620 - 99 (66 * 1.5).
        assert_eq!(r.y + 36 + 900, 1620 - 99);
        // The logical window position is the physical one divided by the scale.
        assert_eq!((r.window_x, r.window_y), (716.0, 390.0));
    }

    #[test]
    fn flyout_rect_on_offset_monitor_adds_its_origin() {
        // Quick settings 300x300, right-aligned 10 px from the right edge of a monitor at x=1920.
        let x = 1920.0 - 10.0 - 300.0;
        let r = flyout_rect(1920, 0, 1920, 1080, 1.0, x, 300.0, 300.0);
        assert_eq!(
            r,
            Placement {
                x: 3506,
                y: 690,
                width: 348,
                height: 348,
                origin_y: 358.0,
                // Relative to the primary monitor's top-left, which is the monitor at x=1920 here.
                window_x: 1586.0,
                window_y: 690.0,
            }
        );
    }

    #[test]
    fn flyout_window_position_is_relative_to_the_primary_monitor() {
        // Primary monitor at the origin: window_x = x - PAD, window_y = monitor_h - 42 - window_h.
        let r = flyout_rect(0, 0, 1920, 1080, 1.0, 740.0, 440.0, 600.0);
        assert_eq!(r.window_x, 740.0 - 24.0);
        assert_eq!(r.window_y, 1080.0 - 42.0 - (600.0 + 48.0));
    }

    #[test]
    fn origin_x_is_anchor_relative_to_window_left() {
        // Icon centre at 1000, panel left at 740: 1000 - 740 + 24 = 284 in the window.
        assert_eq!(origin_x(1000.0, 740.0), 284.0);
    }

    #[test]
    fn panel_names_are_validated() {
        assert_eq!(parse_panel("start"), Ok("start"));
        assert_eq!(parse_panel("search"), Ok("search"));
        assert_eq!(parse_panel("quick_settings"), Ok("quick_settings"));
        assert_eq!(parse_panel("notifications"), Ok("notifications"));
        assert!(parse_panel("Start").is_err());
        assert!(parse_panel("quick-settings").is_err());
        assert!(parse_panel("").is_err());
    }

    #[test]
    fn non_finite_or_negative_geometry_is_rejected() {
        assert!(check_geometry(0.0, 440.0, 600.0, 10.0).is_ok());
        assert!(check_geometry(f64::NAN, 440.0, 600.0, 10.0).is_err());
        assert!(check_geometry(0.0, f64::INFINITY, 600.0, 10.0).is_err());
        assert!(check_geometry(0.0, 440.0, -1.0, 10.0).is_err());
        assert!(check_geometry(0.0, 440.0, 600.0, f64::NEG_INFINITY).is_err());
    }
}

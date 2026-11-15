//! Island window geometry: size, and horizontal centring on the monitor the pill is on.
//! That monitor is the primary one unless fullscreen.rs moves or hides the pill.

use std::sync::Mutex;

use tauri::{AppHandle, LogicalSize, Manager, Monitor, PhysicalPosition, WebviewWindow};

pub const LABEL: &str = "island";
/// Logical sizes (width, height) of the collapsed and expanded pill.
pub const COLLAPSED: (f64, f64) = (236.0, 40.0);
pub const EXPANDED: (f64, f64) = (400.0, 150.0);
/// Gap between the top of the screen and the island, in logical pixels.
const TOP_OFFSET_LOGICAL: f64 = 8.0;

/// A monitor in physical pixels, plus its DPI scale factor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

pub fn screen_of(monitor: &Monitor) -> Screen {
    Screen {
        x: monitor.position().x,
        y: monitor.position().y,
        width: monitor.size().width,
        height: monitor.size().height,
        scale: monitor.scale_factor(),
    }
}

#[derive(Default)]
struct Placement {
    /// Last monitor the island was placed on. None means "primary monitor".
    screen: Option<Screen>,
    expanded: bool,
    /// True while a fullscreen app leaves no free monitor for the pill.
    hidden: bool,
}

/// Tauri-managed placement state. Only the main thread touches the window,
/// and the lock is never held across window calls.
#[derive(Default)]
pub struct IslandPlacement(Mutex<Placement>);

fn with_placement<T>(app: &AppHandle, f: impl FnOnce(&mut Placement) -> T) -> Result<T, String> {
    let state = app.state::<IslandPlacement>();
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    Ok(f(&mut guard))
}

fn window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window(LABEL)
        .ok_or_else(|| "island window not found".to_string())
}

/// Returns (x, y) in physical pixels and (width, height) in logical pixels.
pub fn island_rect(
    monitor_x: i32,
    monitor_y: i32,
    monitor_w: u32,
    scale: f64,
    expanded: bool,
) -> (i32, i32, f64, f64) {
    let (w, h) = if expanded { EXPANDED } else { COLLAPSED };
    let width_px = w * scale;
    let x = f64::from(monitor_x) + (f64::from(monitor_w) - width_px) / 2.0;
    let y = monitor_y + (TOP_OFFSET_LOGICAL * scale).round() as i32;
    (x.round() as i32, y, w, h)
}

fn position(window: &WebviewWindow, screen: Screen, expanded: bool) -> Result<(), String> {
    let (x, y, w, h) = island_rect(screen.x, screen.y, screen.width, screen.scale, expanded);
    window
        .set_size(LogicalSize::new(w, h))
        .map_err(|e| e.to_string())?;
    window
        .set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn place(app: &AppHandle, expanded: bool) -> Result<(), String> {
    let window = window(app)?;
    let known = with_placement(app, |p| {
        p.expanded = expanded;
        p.screen
    })?;
    let screen = match known {
        Some(screen) => screen,
        None => {
            let primary = window
                .primary_monitor()
                .map_err(|e| e.to_string())?
                .ok_or("no primary monitor")?;
            screen_of(&primary)
        }
    };
    position(&window, screen, expanded)
}

/// Positions the collapsed island, then shows it (the window starts hidden).
pub fn show_initial(app: &AppHandle) -> Result<(), String> {
    place(app, false)?;
    window(app)?.show().map_err(|e| e.to_string())
}

/// Moves the island onto `target` and shows it, or hides it when `target` is None.
/// Does nothing when the placement already matches the request.
pub fn set_target(app: &AppHandle, target: Option<Screen>) -> Result<(), String> {
    let window = window(app)?;
    // Some(was_hidden) when the placement changed, None when nothing needs doing.
    let changed = with_placement(app, |p| match target {
        None => {
            if p.hidden {
                return None;
            }
            p.hidden = true;
            Some(false)
        }
        Some(screen) => {
            if !p.hidden && p.screen == Some(screen) {
                return None;
            }
            let was_hidden = p.hidden;
            p.hidden = false;
            p.screen = Some(screen);
            Some(was_hidden)
        }
    })?;
    let Some(was_hidden) = changed else {
        return Ok(());
    };
    match target {
        None => window.hide().map_err(|e| e.to_string()),
        Some(screen) => {
            let expanded = with_placement(app, |p| p.expanded)?;
            position(&window, screen, expanded)?;
            if was_hidden {
                window.show().map_err(|e| e.to_string())
            } else {
                Ok(())
            }
        }
    }
}

#[tauri::command]
pub fn set_expanded(app: AppHandle, expanded: bool) -> Result<(), String> {
    place(&app, expanded)
}

#[cfg(test)]
mod tests {
    use super::island_rect;

    #[test]
    fn collapsed_is_centred_on_primary_at_100_percent() {
        assert_eq!(island_rect(0, 0, 1920, 1.0, false), (842, 8, 236.0, 40.0));
    }

    #[test]
    fn expanded_is_centred_at_200_percent() {
        assert_eq!(island_rect(0, 0, 3840, 2.0, true), (1520, 16, 400.0, 150.0));
    }

    #[test]
    fn offset_monitor_adds_its_origin() {
        assert_eq!(island_rect(1920, 0, 1920, 1.0, false).0, 2762);
    }
}

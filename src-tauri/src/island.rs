//! Island window geometry: size, and horizontal centring on the primary monitor.

use tauri::{AppHandle, LogicalSize, Manager, PhysicalPosition};

const LABEL: &str = "island";
/// Logical sizes (width, height) of the collapsed and expanded pill.
pub const COLLAPSED: (f64, f64) = (236.0, 40.0);
pub const EXPANDED: (f64, f64) = (400.0, 150.0);
/// Gap between the top of the screen and the island, in logical pixels.
const TOP_OFFSET_LOGICAL: f64 = 8.0;

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

fn place(app: &AppHandle, expanded: bool) -> Result<(), String> {
    let window = app
        .get_webview_window(LABEL)
        .ok_or("island window not found")?;
    let monitor = window
        .primary_monitor()
        .map_err(|e| e.to_string())?
        .ok_or("no primary monitor")?;
    let origin = monitor.position();
    let (x, y, w, h) = island_rect(
        origin.x,
        origin.y,
        monitor.size().width,
        monitor.scale_factor(),
        expanded,
    );
    window
        .set_size(LogicalSize::new(w, h))
        .map_err(|e| e.to_string())?;
    window
        .set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Positions the collapsed island, then shows it (the window starts hidden).
pub fn show_initial(app: &AppHandle) -> Result<(), String> {
    place(app, false)?;
    let window = app
        .get_webview_window(LABEL)
        .ok_or("island window not found")?;
    window.show().map_err(|e| e.to_string())
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

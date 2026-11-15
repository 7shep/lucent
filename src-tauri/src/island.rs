//! Island window geometry: size, and horizontal centring on the monitor the pill is on.
//! That monitor is the primary one unless fullscreen.rs moves or hides the pill.

use std::sync::Mutex;

use tauri::{AppHandle, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewWindow};
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE};

pub const LABEL: &str = "island";
/// Logical sizes (width, height) of the collapsed and expanded pill.
pub const COLLAPSED: (f64, f64) = (236.0, 40.0);
pub const EXPANDED: (f64, f64) = (400.0, 150.0);
/// Logical size of the empty-state capsule (nothing playing). Drawn inside the collapsed window.
pub const EMPTY: (f64, f64) = (84.0, 24.0);
/// Corner radii (logical) matching the CSS border-radius of each state.
const COLLAPSED_RADIUS: f64 = 20.0;
const EXPANDED_RADIUS: f64 = 28.0;
const EMPTY_RADIUS: f64 = 12.0;
/// Gap between the top of the screen and the island, in logical pixels.
const TOP_OFFSET_LOGICAL: f64 = 8.0;

/// Native hit region in physical pixels, relative to the window's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub radius: i32,
}

/// Logical (size, corner radius) of the drawn pill. Expanded wins over empty, as in the CSS.
pub fn pill_shape(expanded: bool, empty: bool) -> ((f64, f64), f64) {
    if expanded {
        (EXPANDED, EXPANDED_RADIUS)
    } else if empty {
        (EMPTY, EMPTY_RADIUS)
    } else {
        (COLLAPSED, COLLAPSED_RADIUS)
    }
}

/// Logical size of the native window. Only the expanded state resizes it.
fn window_logical(expanded: bool) -> (f64, f64) {
    if expanded {
        EXPANDED
    } else {
        COLLAPSED
    }
}

/// Region for the pill in state (expanded, empty) at DPI `scale`.
pub fn region_for(expanded: bool, empty: bool, scale: f64) -> HitRegion {
    let window_px = (window_logical(expanded).0 * scale).round() as i32;
    let (pill, radius) = pill_shape(expanded, empty);
    hit_region(window_px, pill, radius, scale)
}

/// Region for a pill of `pill` logical size, drawn at the top and centred in a window
/// `window_width_px` physical pixels wide. Everything is scaled by `scale`.
pub fn hit_region(window_width_px: i32, pill: (f64, f64), radius: f64, scale: f64) -> HitRegion {
    let width = (pill.0 * scale).round() as i32;
    let height = (pill.1 * scale).round() as i32;
    let x = ((f64::from(window_width_px) - f64::from(width)) / 2.0).round() as i32;
    HitRegion {
        x,
        y: 0,
        width,
        height,
        radius: (radius * scale).round() as i32,
    }
}

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
    /// Last pill shape (expanded, empty) passed to set_hit_region. None until the
    /// UI first sets a region; until then the whole window is clickable.
    region_shape: Option<(bool, bool)>,
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

/// Physical size of the pill on a screen with the given scale factor.
pub fn physical_size(width_logical: f64, height_logical: f64, scale: f64) -> (u32, u32) {
    (
        (width_logical * scale).round() as u32,
        (height_logical * scale).round() as u32,
    )
}

/// Moves and resizes in physical pixels on the target screen's scale. Setting the
/// size can cross a DPI boundary and shift the window, so the position is set
/// again afterwards.
fn position(window: &WebviewWindow, screen: Screen, expanded: bool) -> Result<(), String> {
    let (x, y, w, h) = island_rect(screen.x, screen.y, screen.width, screen.scale, expanded);
    let (pw, ph) = physical_size(w, h, screen.scale);
    window
        .set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    window
        .set_size(PhysicalSize::new(pw, ph))
        .map_err(|e| e.to_string())?;
    window
        .set_position(PhysicalPosition::new(x, y))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Moves the island onto `screen`, then re-applies the hit region at that screen's
/// scale. The region is in physical pixels, so it goes stale on a DPI change.
fn reposition(app: &AppHandle, window: &WebviewWindow, screen: Screen, expanded: bool) -> Result<(), String> {
    position(window, screen, expanded)?;
    reapply_region(app, window, screen.scale)
}

/// Re-applies the last hit region the UI asked for, at `scale`. No-op before the
/// UI has set one.
fn reapply_region(app: &AppHandle, window: &WebviewWindow, scale: f64) -> Result<(), String> {
    let Some((expanded, empty)) = with_placement(app, |p| p.region_shape)? else {
        return Ok(());
    };
    apply_native_region(window, region_for(expanded, empty, scale))
}

/// Called when the island window's DPI scale changes (display scaling changed, or
/// the window landed on a monitor with a different scale).
pub fn on_scale_changed(app: &AppHandle, scale: f64) {
    let result = window(app).and_then(|window| reapply_region(app, &window, scale));
    if let Err(err) = result {
        eprintln!("lucent: hit region refresh failed: {err}");
    }
}

/// Shows the window without activating it, so it never takes focus from a game.
fn show_without_activating(window: &WebviewWindow) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|e| e.to_string())?;
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    Ok(())
}

/// The screen the pill is shown on, or None while it is hidden.
pub fn shown_screen(app: &AppHandle) -> Result<Option<Screen>, String> {
    with_placement(app, |p| if p.hidden { None } else { p.screen })
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
    // Remember the resolved screen so later decisions compare against it.
    with_placement(app, |p| p.screen = Some(screen))?;
    // Same monitor, so the region's scale still holds. The UI updates the region
    // itself around expand/collapse, in the order that avoids clipping.
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
            reposition(app, &window, screen, expanded)?;
            if was_hidden {
                show_without_activating(&window)
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

/// Clips the native window to the drawn pill, so clicks outside it reach windows below.
/// The caller must grow the region before the pill grows, and shrink it only after the
/// pill has shrunk. Otherwise drawn pixels get clipped.
#[tauri::command]
pub fn set_hit_region(app: AppHandle, expanded: bool, empty: bool) -> Result<(), String> {
    let window = window(&app)?;
    // Remembered so a later move to another monitor can rebuild it at the new scale.
    with_placement(&app, |p| p.region_shape = Some((expanded, empty)))?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    apply_native_region(&window, region_for(expanded, empty, scale))
}

#[cfg(windows)]
fn apply_native_region(window: &WebviewWindow, region: HitRegion) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, DeleteObject, SetWindowRgn, HGDIOBJ};

    let raw = window.hwnd().map_err(|e| e.to_string())?;
    // Convert through the raw pointer, so this works whatever `windows` version tauri uses.
    let hwnd = HWND(raw.0 as isize as *mut core::ffi::c_void);
    // SAFETY: hwnd is the island window's handle. On success SetWindowRgn takes ownership of
    // the region, so it is only deleted on failure.
    unsafe {
        let rgn = CreateRoundRectRgn(
            region.x,
            region.y,
            region.x + region.width,
            region.y + region.height,
            region.radius * 2,
            region.radius * 2,
        );
        if rgn.is_invalid() {
            return Err("CreateRoundRectRgn failed".into());
        }
        if SetWindowRgn(hwnd, Some(rgn), true) == 0 {
            let _ = DeleteObject(HGDIOBJ(rgn.0));
            return Err("SetWindowRgn failed".into());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn apply_native_region(_window: &WebviewWindow, _region: HitRegion) -> Result<(), String> {
    Ok(())
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

    #[test]
    fn physical_size_at_100_percent_matches_logical() {
        assert_eq!(super::physical_size(236.0, 40.0, 1.0), (236, 40));
        assert_eq!(super::physical_size(400.0, 150.0, 1.0), (400, 150));
    }

    #[test]
    fn physical_size_at_150_percent_scales_both_axes() {
        assert_eq!(super::physical_size(236.0, 40.0, 1.5), (354, 60));
        assert_eq!(super::physical_size(400.0, 150.0, 1.5), (600, 225));
    }
}

#[cfg(test)]
mod region_tests {
    use super::{hit_region, pill_shape, region_for, HitRegion, COLLAPSED, EMPTY, EXPANDED};

    #[test]
    fn region_for_tracks_scale_when_moving_monitors() {
        // Same pill state on a 100% and a 150% monitor.
        assert_eq!(
            region_for(false, false, 1.0),
            HitRegion { x: 0, y: 0, width: 236, height: 40, radius: 20 }
        );
        assert_eq!(
            region_for(false, false, 1.5),
            HitRegion { x: 0, y: 0, width: 354, height: 60, radius: 30 }
        );
        assert_eq!(
            region_for(false, true, 1.5),
            HitRegion { x: 114, y: 0, width: 126, height: 36, radius: 18 }
        );
        assert_eq!(
            region_for(true, true, 1.5),
            HitRegion { x: 0, y: 0, width: 600, height: 225, radius: 42 }
        );
    }

    #[test]
    fn collapsed_region_matches_pill_at_100_percent() {
        let r = hit_region(236, COLLAPSED, 20.0, 1.0);
        assert_eq!(r, HitRegion { x: 0, y: 0, width: 236, height: 40, radius: 20 });
    }

    #[test]
    fn collapsed_region_scales_at_200_percent() {
        let r = hit_region(472, COLLAPSED, 20.0, 2.0);
        assert_eq!(r, HitRegion { x: 0, y: 0, width: 472, height: 80, radius: 40 });
    }

    #[test]
    fn empty_capsule_is_centred_at_100_percent() {
        let r = hit_region(236, EMPTY, 12.0, 1.0);
        assert_eq!(r, HitRegion { x: 76, y: 0, width: 84, height: 24, radius: 12 });
    }

    #[test]
    fn empty_capsule_is_centred_at_200_percent() {
        let r = hit_region(472, EMPTY, 12.0, 2.0);
        assert_eq!(r, HitRegion { x: 152, y: 0, width: 168, height: 48, radius: 24 });
    }

    #[test]
    fn expanded_region_fills_window_at_100_percent() {
        let r = hit_region(400, EXPANDED, 28.0, 1.0);
        assert_eq!(r, HitRegion { x: 0, y: 0, width: 400, height: 150, radius: 28 });
    }

    #[test]
    fn expanded_region_fills_window_at_200_percent() {
        let r = hit_region(800, EXPANDED, 28.0, 2.0);
        assert_eq!(r, HitRegion { x: 0, y: 0, width: 800, height: 300, radius: 56 });
    }

    #[test]
    fn expanded_wins_over_empty() {
        assert_eq!(pill_shape(true, true), (EXPANDED, 28.0));
        assert_eq!(pill_shape(false, true), (EMPTY, 12.0));
        assert_eq!(pill_shape(false, false), (COLLAPSED, 20.0));
    }
}

//! Glass support for the dock and flyout windows. The CSS glass blurs the desktop
//! wallpaper, so this module supplies that wallpaper (`get_backdrop`), tells the UI when
//! it changes (`backdrop_changed`), and clips each window to the rounded-rect regions the
//! UI reports (`set_regions`). See docs/taskbar-spec.md, module `glass`.

use std::cell::Cell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::Value;
use tauri::{AppHandle, Emitter, WebviewWindow};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{GENERIC_READ, HWND};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, GetDC, GetDeviceCaps,
    GetSysColor, ReleaseDC, SetWindowRgn, COLOR_DESKTOP, HGDIOBJ, LOGPIXELSX, RGN_ERROR, RGN_OR,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, IWICBitmapSource, IWICImagingFactory, IWICBitmapScaler,
    WICBitmapInterpolationModeFant, WICConvertBitmapSource, WICDecodeMetadataCacheOnDemand,
    GUID_WICPixelFormat32bppRGBA,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, REG_DWORD, REG_SZ, REG_VALUE_TYPE, RRF_RT_ANY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SystemParametersInfoW, SM_CXSCREEN, SM_CYSCREEN, SPI_GETDESKWALLPAPER,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

use crate::model::{Backdrop, Rect};

/// Longest side of the wallpaper image sent to the UI, in px.
const MAX_IMAGE_SIDE: u32 = 640;
const DESKTOP_KEY: &str = r"Control Panel\Desktop";
const COLORS_KEY: &str = r"Control Panel\Colors";
/// Capacity of the SPI_GETDESKWALLPAPER buffer, in UTF-16 units.
const WALLPAPER_PATH_UNITS: usize = 1024;
/// Dots per inch that Windows counts as 100 %.
const BASE_DPI: f64 = 96.0;

/// Cached backdrop, cleared by `backdrop_changed`.
static BACKDROP: Mutex<Option<Backdrop>> = Mutex::new(None);
/// Bumped on every change. A build that started before a change does not store its result.
static GENERATION: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// Backdrop layout name for the registry's `WallpaperStyle` and `TileWallpaper` values.
/// A missing or unknown style is "fill", which is what Windows uses by default.
pub fn style_name(wallpaper_style: Option<&str>, tile: Option<&str>) -> &'static str {
    let Some(style) = wallpaper_style.and_then(|s| s.trim().parse::<u32>().ok()) else {
        return "fill";
    };
    match style {
        10 => "fill",
        6 => "fit",
        2 => "stretch",
        22 => "span",
        0 => {
            if tile.and_then(|t| t.trim().parse::<u32>().ok()) == Some(1) {
                "tile"
            } else {
                "center"
            }
        }
        _ => "fill",
    }
}

/// Parses a `Control Panel\Colors` value ("R G B", decimal) into "#rrggbb".
/// None unless there are exactly three channels in 0..=255.
pub fn parse_colour(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [r, g, b] = parts.as_slice() else {
        return None;
    };
    let r: u8 = r.parse().ok()?;
    let g: u8 = g.parse().ok()?;
    let b: u8 = b.parse().ok()?;
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}

/// Converts a COLORREF (0x00BBGGRR) to "#rrggbb".
pub fn colorref_to_hex(colorref: u32) -> String {
    let r = colorref & 0xff;
    let g = (colorref >> 8) & 0xff;
    let b = (colorref >> 16) & 0xff;
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Size of `width` x `height` scaled so the longer side is at most `max`, keeping the
/// aspect ratio (rounded to nearest, never below 1 px). Images already within `max` are
/// returned unchanged. A zero size gives (0, 0).
pub fn scaled_dims(width: u32, height: u32, max: u32) -> (u32, u32) {
    if width == 0 || height == 0 || max == 0 {
        return (0, 0);
    }
    let long = width.max(height);
    if long <= max {
        return (width, height);
    }
    let other = |side: u32| -> u32 {
        let v = (u64::from(side) * u64::from(max) + u64::from(long) / 2) / u64::from(long);
        (v as u32).max(1)
    };
    if width >= height {
        (max, other(height))
    } else {
        (other(width), max)
    }
}

/// Logical size of a monitor from its pixel size and DPI. Scale is DPI / 96.
pub fn logical_size(px_width: i32, px_height: i32, dpi: i32) -> (f64, f64) {
    let scale = if dpi > 0 {
        f64::from(dpi) / BASE_DPI
    } else {
        1.0
    };
    (f64::from(px_width) / scale, f64::from(px_height) / scale)
}

/// Joins a NUL-terminated UTF-16 buffer (or the whole buffer when it has no NUL).
fn text_from_wide(buf: &[u16]) -> Option<String> {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16(&buf[..len]).ok()
}

// ---------------------------------------------------------------------------
// Small Windows helpers
// ---------------------------------------------------------------------------

/// Locks a mutex, recovering the data if another thread panicked while holding it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Initialises COM as apartment-threaded on the calling thread, once per thread.
/// S_FALSE and RPC_E_CHANGED_MODE are both fine: the thread already has a mode and COM
/// calls still work. The apartment is never torn down, since pool threads are reused.
fn ensure_com() {
    COM_READY.with(|ready| {
        if !ready.get() {
            // SAFETY: CoInitializeEx with a null pvReserved has no preconditions beyond
            // running on this thread. The HRESULT is ignored on purpose.
            let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            ready.set(true);
        }
    });
}

/// Wide, NUL-terminated copy of `s`. None when `s` contains a NUL.
fn wide_checked(s: &str) -> Option<Vec<u16>> {
    if s.contains('\0') {
        None
    } else {
        Some(s.encode_utf16().chain(std::iter::once(0)).collect())
    }
}

/// Wide, NUL-terminated copy of a known-good literal.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The live HWND of a Tauri window, converted the same way as island.rs.
fn hwnd_of(window: &WebviewWindow) -> Result<HWND, String> {
    let raw = window.hwnd().map_err(|e| e.to_string())?;
    Ok(HWND(raw.0 as isize as *mut c_void))
}

/// Reads one value under HKCU as text. REG_SZ is returned as is, REG_DWORD as decimal.
/// None when the value is missing or of another type.
fn registry_text(subkey: &str, name: &str) -> Option<String> {
    let key = wide(subkey);
    let value = wide(name);
    let mut kind = REG_VALUE_TYPE(0);
    let mut buf = vec![0u8; 1024];
    let mut len = u32::try_from(buf.len()).ok()?;
    // SAFETY: key and value are NUL-terminated and live across the call. buf has `len`
    // writable bytes, and kind and len are valid out-pointers that the call writes.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_ANY,
            Some(&mut kind as *mut REG_VALUE_TYPE),
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len as *mut u32),
        )
    };
    if status.is_err() {
        return None;
    }
    let used = (len as usize).min(buf.len());
    if kind == REG_DWORD && used >= 4 {
        Some(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]).to_string())
    } else if kind == REG_SZ {
        let units: Vec<u16> = buf[..used]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        text_from_wide(&units)
    } else {
        None
    }
}

/// Path of the current desktop wallpaper, if it names an existing file.
fn wallpaper_path() -> Option<PathBuf> {
    let mut buf = vec![0u16; WALLPAPER_PATH_UNITS];
    // SAFETY: buf is a writable buffer of WALLPAPER_PATH_UNITS UTF-16 units, and uiParam
    // gives its length in characters, as SPI_GETDESKWALLPAPER requires. Nothing is set.
    unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            WALLPAPER_PATH_UNITS as u32,
            Some(buf.as_mut_ptr().cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()?;
    let path = PathBuf::from(text_from_wide(&buf)?);
    path.is_file().then_some(path)
}

/// Logical size of the primary monitor. GetSystemMetrics and GetDeviceCaps report in the
/// same space (physical in a DPI-aware process, virtualised otherwise), so the division by
/// the DPI scale gives logical px in both cases.
fn primary_logical_size() -> (f64, f64) {
    // SAFETY: GetSystemMetrics reads no pointers and has no preconditions.
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    // SAFETY: GetDC(None) returns the screen DC, released below on every path.
    let hdc = unsafe { GetDC(None) };
    let dpi = if hdc.is_invalid() {
        BASE_DPI as i32
    } else {
        // SAFETY: hdc is the valid screen DC from GetDC above, and LOGPIXELSX is a valid index.
        let dpi = unsafe { GetDeviceCaps(Some(hdc), LOGPIXELSX) };
        // SAFETY: hdc came from GetDC(None) above and is released exactly once.
        unsafe { ReleaseDC(None, hdc) };
        dpi
    };
    logical_size(w, h, dpi)
}

/// Converts a COLORREF from GetSysColor to "#rrggbb".
fn desktop_colour_fallback() -> String {
    // SAFETY: GetSysColor reads no pointers. COLOR_DESKTOP is a valid index.
    let colorref = unsafe { GetSysColor(COLOR_DESKTOP) };
    colorref_to_hex(colorref)
}

// ---------------------------------------------------------------------------
// Wallpaper decoding (WIC)
// ---------------------------------------------------------------------------

/// A decoded wallpaper as PNG, with its pixel size.
struct Decoded {
    png: Vec<u8>,
    width: u32,
    height: u32,
}

/// Encodes straight RGBA8 pixels as a PNG. None when the sizes do not match or the encoder fails.
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let expected = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)?;
    if expected == 0 || rgba.len() != expected {
        return None;
    }
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
        writer.finish().ok()?;
    }
    Some(buf)
}

/// Decodes the first frame of the image at `path` with WIC, scales it so the longer side
/// is at most MAX_IMAGE_SIDE, and encodes it as PNG. None for any failure.
fn decode_wallpaper(path: &Path) -> Option<Decoded> {
    ensure_com();
    let wide_path = wide_checked(path.to_str()?)?;
    // SAFETY: CLSID_WICImagingFactory is a valid class id. The returned interface is owned by us.
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }.ok()?;
    // SAFETY: wide_path is NUL-terminated and lives across the call. A null vendor GUID is allowed.
    let decoder = unsafe {
        factory.CreateDecoderFromFilename(
            PCWSTR(wide_path.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )
    }
    .ok()?;
    // SAFETY: the decoder is a live interface and frame 0 is the image's first frame.
    let frame = unsafe { decoder.GetFrame(0) }.ok()?;
    let (mut width, mut height) = (0u32, 0u32);
    // SAFETY: both out-pointers are valid locals for the duration of the call.
    unsafe { frame.GetSize(&mut width, &mut height) }.ok()?;
    let (out_w, out_h) = scaled_dims(width, height, MAX_IMAGE_SIDE);
    if out_w == 0 || out_h == 0 {
        return None;
    }
    let source: IWICBitmapSource = frame.cast().ok()?;
    let source: IWICBitmapSource = if (out_w, out_h) == (width, height) {
        source
    } else {
        // SAFETY: the factory is live. The scaler is owned by us and read back as a bitmap source.
        let scaler: IWICBitmapScaler = unsafe { factory.CreateBitmapScaler() }.ok()?;
        // SAFETY: source is a live bitmap source, and the target size is non-zero.
        unsafe { scaler.Initialize(&source, out_w, out_h, WICBitmapInterpolationModeFant) }
            .ok()?;
        scaler.cast().ok()?
    };
    // SAFETY: source is a live bitmap source. The converter is owned by us.
    let rgba = unsafe { WICConvertBitmapSource(&GUID_WICPixelFormat32bppRGBA, &source) }.ok()?;
    let stride = out_w.checked_mul(4)?;
    let mut pixels = vec![0u8; usize::try_from(stride).ok()?.checked_mul(out_h as usize)?];
    // SAFETY: a null rect copies the whole image. pixels holds stride * out_h bytes, which is
    // what the call writes at this stride.
    unsafe { rgba.CopyPixels(std::ptr::null(), stride, &mut pixels) }.ok()?;
    let png = encode_png(out_w, out_h, &pixels)?;
    Some(Decoded {
        png,
        width: out_w,
        height: out_h,
    })
}

/// Reads the whole backdrop: wallpaper image, layout, colour and monitor size. Never
/// fails: a missing or corrupt wallpaper just gives `image: None`. Also returns the decoded
/// image for the probe.
fn read_backdrop() -> (Backdrop, Option<Decoded>) {
    let decoded = wallpaper_path().and_then(|path| decode_wallpaper(&path));
    let style = style_name(
        registry_text(DESKTOP_KEY, "WallpaperStyle").as_deref(),
        registry_text(DESKTOP_KEY, "TileWallpaper").as_deref(),
    );
    let color = registry_text(COLORS_KEY, "Background")
        .and_then(|text| parse_colour(&text))
        .unwrap_or_else(desktop_colour_fallback);
    let (monitor_width, monitor_height) = primary_logical_size();
    let image = decoded.as_ref().map(|d| {
        format!("data:image/png;base64,{}", STANDARD.encode(&d.png))
    });
    (
        Backdrop {
            image,
            style: style.to_string(),
            color,
            monitor_width,
            monitor_height,
        },
        decoded,
    )
}

// ---------------------------------------------------------------------------
// Commands and events
// ---------------------------------------------------------------------------

/// The desktop wallpaper for the CSS glass. Cached until `backdrop_changed` runs.
#[tauri::command]
pub async fn get_backdrop(_app: AppHandle) -> Result<Backdrop, String> {
    let cached = lock(&BACKDROP).clone();
    if let Some(hit) = cached {
        return Ok(hit);
    }
    let generation = GENERATION.load(Ordering::SeqCst);
    let backdrop = tauri::async_runtime::spawn_blocking(|| read_backdrop().0)
        .await
        .map_err(|e| e.to_string())?;
    {
        let mut cache = lock(&BACKDROP);
        // Stored only when no change happened while this was being built.
        if GENERATION.load(Ordering::SeqCst) == generation {
            *cache = Some(backdrop.clone());
        }
    }
    Ok(backdrop)
}

/// The desktop wallpaper or the display changed: drops the cached backdrop and tells the
/// dock and flyout to reload it. Cheap, so it can run on the main thread.
pub fn backdrop_changed(app: &AppHandle) {
    {
        let mut cache = lock(&BACKDROP);
        // Bumped under the lock, so an in-flight build cannot store a stale result.
        GENERATION.fetch_add(1, Ordering::SeqCst);
        *cache = None;
    }
    for label in [crate::shell::DOCK_LABEL, crate::flyout::LABEL] {
        if let Err(err) = app.emit_to(label, "backdrop-changed", ()) {
            eprintln!("lucent: backdrop-changed emit to {label} failed: {err}");
        }
    }
}

/// `lucent --print-backdrop`: prints the backdrop as JSON, with the image replaced by a
/// size summary. Runs without a window.
pub fn print_probe() {
    let (backdrop, decoded) = read_backdrop();
    let mut value: Value = match serde_json::to_value(&backdrop) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("lucent: cannot encode the backdrop: {err}");
            return;
        }
    };
    if let (Some(d), Some(object)) = (decoded, value.as_object_mut()) {
        object.insert(
            "image".into(),
            Value::String(format!("<{} bytes png {}x{}>", d.png.len(), d.width, d.height)),
        );
    }
    println!("{value}");
}

// ---------------------------------------------------------------------------
// Window regions
// ---------------------------------------------------------------------------

/// A region rectangle in physical pixels, relative to the window's top-left corner.
/// `ellipse` is the corner ellipse's width and height (twice the radius).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub ellipse: i32,
}

/// Largest physical coordinate a region edge may have (any real screen fits).
const COORD_LIMIT: f64 = 1_000_000.0;

/// Converts logical rects to physical ones at `scale`. Edges are rounded separately, so
/// neighbouring rects meet without gaps. Rects that round to nothing are dropped.
pub fn physical_rects(rects: &[Rect], scale: f64) -> Result<Vec<PxRect>, String> {
    if !scale.is_finite() || scale <= 0.0 {
        return Err(format!("invalid scale factor {scale}"));
    }
    let mut out = Vec::with_capacity(rects.len());
    for r in rects {
        if ![r.x, r.y, r.w, r.h, r.r].iter().all(|v| v.is_finite()) {
            return Err("region has a non-finite value".into());
        }
        // Clamped far inside i32, so the subtractions below can never overflow.
        let px = |v: f64| (v * scale).round().clamp(-COORD_LIMIT, COORD_LIMIT) as i32;
        let left = px(r.x);
        let top = px(r.y);
        let right = px(r.x + r.w);
        let bottom = px(r.y + r.h);
        let width = right - left;
        let height = bottom - top;
        if width <= 0 || height <= 0 {
            continue;
        }
        let ellipse = ((2.0 * r.r * scale).round() as i32).clamp(0, width.min(height));
        out.push(PxRect {
            x: left,
            y: top,
            width,
            height,
            ellipse,
        });
    }
    Ok(out)
}

/// Builds the union of `rects` as one region and installs it on `hwnd`.
fn apply_region(hwnd: HWND, rects: &[PxRect]) -> Result<(), String> {
    // SAFETY: the Gdi calls only allocate region objects and read their integer arguments.
    // Ownership: each source region is deleted once after CombineRgn (which copies it). The
    // accumulator is deleted on every failure path. After SetWindowRgn succeeds the system
    // owns it, so it is not deleted then.
    unsafe {
        let acc = CreateRectRgn(0, 0, 0, 0);
        if acc.is_invalid() {
            return Err("CreateRectRgn failed".into());
        }
        for r in rects {
            let rgn = CreateRoundRectRgn(
                r.x,
                r.y,
                r.x + r.width,
                r.y + r.height,
                r.ellipse,
                r.ellipse,
            );
            if rgn.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(acc.0));
                return Err("CreateRoundRectRgn failed".into());
            }
            let kind = CombineRgn(Some(acc), Some(acc), Some(rgn), RGN_OR);
            let _ = DeleteObject(HGDIOBJ(rgn.0));
            if kind == RGN_ERROR {
                let _ = DeleteObject(HGDIOBJ(acc.0));
                return Err("CombineRgn failed".into());
            }
        }
        if SetWindowRgn(hwnd, Some(acc), true) == 0 {
            let _ = DeleteObject(HGDIOBJ(acc.0));
            return Err("SetWindowRgn failed".into());
        }
    }
    Ok(())
}

/// Clips `window` to the union of `rects` (logical px). An empty slice clips everything.
pub fn set_region_rects(window: &WebviewWindow, rects: &[Rect]) -> Result<(), String> {
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let px = physical_rects(rects, scale)?;
    let hwnd = hwnd_of(window)?;
    apply_region(hwnd, &px)
}

/// Clips the calling window to the union of `rects` (logical px). Clicks and
/// the backdrop only exist inside the union. An empty list clips everything.
#[tauri::command]
pub fn set_regions(window: WebviewWindow, rects: Vec<Rect>) -> Result<(), String> {
    set_region_rects(&window, &rects)
}

#[cfg(test)]
mod tests {
    use super::{colorref_to_hex, logical_size, parse_colour, physical_rects, scaled_dims, style_name, PxRect};
    use crate::model::Rect;
    use std::path::{Path, PathBuf};

    fn rect(x: f64, y: f64, w: f64, h: f64, r: f64) -> Rect {
        Rect { x, y, w, h, r }
    }

    #[test]
    fn regions_pass_through_at_100_percent() {
        let out = physical_rects(&[rect(10.0, 4.0, 44.0, 40.0, 20.0)], 1.0).unwrap_or_default();
        assert_eq!(
            out,
            vec![PxRect { x: 10, y: 4, width: 44, height: 40, ellipse: 40 }]
        );
    }

    #[test]
    fn regions_scale_and_round_edges_at_150_percent() {
        // Left 10.2 -> 15.3 -> 15; right 54.2 -> 81.3 -> 81; width 66.
        let out = physical_rects(&[rect(10.2, 0.0, 44.0, 40.0, 20.0)], 1.5).unwrap_or_default();
        assert_eq!(
            out,
            vec![PxRect { x: 15, y: 0, width: 66, height: 60, ellipse: 60 }]
        );
    }

    #[test]
    fn ellipse_is_clamped_to_the_smaller_side() {
        let out = physical_rects(&[rect(0.0, 0.0, 100.0, 20.0, 50.0)], 1.0).unwrap_or_default();
        assert_eq!(out[0].ellipse, 20);
    }

    #[test]
    fn empty_and_degenerate_rects_are_dropped() {
        let out = physical_rects(
            &[rect(0.0, 0.0, 0.0, 10.0, 0.0), rect(0.0, 0.0, 10.0, 0.0, 0.0)],
            1.0,
        );
        assert_eq!(out, Ok(vec![]));
    }

    #[test]
    fn non_finite_input_is_rejected() {
        assert!(physical_rects(&[rect(f64::NAN, 0.0, 1.0, 1.0, 0.0)], 1.0).is_err());
        assert!(physical_rects(&[rect(0.0, 0.0, f64::INFINITY, 1.0, 0.0)], 1.0).is_err());
        assert!(physical_rects(&[], f64::NAN).is_err());
        assert!(physical_rects(&[], 0.0).is_err());
    }

    #[test]
    fn wallpaper_style_maps_registry_values() {
        assert_eq!(style_name(Some("10"), Some("0")), "fill");
        assert_eq!(style_name(Some("6"), Some("0")), "fit");
        assert_eq!(style_name(Some("2"), Some("0")), "stretch");
        assert_eq!(style_name(Some("22"), Some("0")), "span");
        assert_eq!(style_name(Some("0"), Some("1")), "tile");
        assert_eq!(style_name(Some("0"), Some("0")), "center");
        assert_eq!(style_name(Some("0"), None), "center");
        // Tile only counts with style 0.
        assert_eq!(style_name(Some("10"), Some("1")), "fill");
        // Anything else, and a missing style, is fill.
        assert_eq!(style_name(Some("4"), Some("0")), "fill");
        assert_eq!(style_name(Some("abc"), None), "fill");
        assert_eq!(style_name(None, None), "fill");
    }

    #[test]
    fn colour_value_parses_to_hex() {
        assert_eq!(parse_colour("0 0 0").as_deref(), Some("#000000"));
        assert_eq!(parse_colour("255 128 1").as_deref(), Some("#ff8001"));
        assert_eq!(parse_colour("  0 120 215  ").as_deref(), Some("#0078d7"));
    }

    #[test]
    fn malformed_colour_values_are_rejected() {
        assert_eq!(parse_colour(""), None);
        assert_eq!(parse_colour("1 2"), None);
        assert_eq!(parse_colour("1 2 3 4"), None);
        assert_eq!(parse_colour("256 0 0"), None);
        assert_eq!(parse_colour("-1 0 0"), None);
        assert_eq!(parse_colour("a b c"), None);
    }

    #[test]
    fn colorref_is_bgr_packed() {
        // 0x00BBGGRR: blue 0xFF, green 0x80, red 0x01.
        assert_eq!(colorref_to_hex(0x00ff_8001), "#0180ff");
        assert_eq!(colorref_to_hex(0), "#000000");
    }

    #[test]
    fn image_within_the_limit_keeps_its_size() {
        assert_eq!(scaled_dims(640, 360, 640), (640, 360));
        assert_eq!(scaled_dims(100, 50, 640), (100, 50));
        assert_eq!(scaled_dims(640, 640, 640), (640, 640));
    }

    #[test]
    fn longer_side_is_scaled_to_the_limit_keeping_aspect() {
        assert_eq!(scaled_dims(1920, 1080, 640), (640, 360));
        assert_eq!(scaled_dims(1080, 1920, 640), (360, 640));
        assert_eq!(scaled_dims(3000, 2000, 640), (640, 427));
        assert_eq!(scaled_dims(5120, 2880, 640), (640, 360));
    }

    #[test]
    fn extreme_aspect_ratios_keep_at_least_one_pixel() {
        assert_eq!(scaled_dims(1, 10000, 640), (1, 640));
        assert_eq!(scaled_dims(10000, 1, 640), (640, 1));
    }

    #[test]
    fn zero_sizes_give_zero() {
        assert_eq!(scaled_dims(0, 1080, 640), (0, 0));
        assert_eq!(scaled_dims(1920, 0, 640), (0, 0));
    }

    #[test]
    fn monitor_size_is_logical_at_the_dpi_scale() {
        assert_eq!(logical_size(1920, 1080, 96), (1920.0, 1080.0));
        assert_eq!(logical_size(2880, 1620, 144), (1920.0, 1080.0));
        assert_eq!(logical_size(1920, 1080, 0), (1920.0, 1080.0));
    }

    fn find_jpg(root: &Path) -> Option<PathBuf> {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("jpg"))
                {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Needs a real wallpaper image on this machine. Run with `-- --ignored`.
    #[test]
    #[ignore = "needs a JPEG under C:\\Windows\\Web; run with -- --ignored"]
    fn wic_decodes_a_real_wallpaper_within_640_px() {
        let preferred = Path::new(r"C:\Windows\Web\Wallpaper\Windows\img0.jpg");
        let path = if preferred.is_file() {
            preferred.to_path_buf()
        } else {
            let Some(found) = find_jpg(Path::new(r"C:\Windows\Web")) else {
                panic!("no .jpg under C:\\Windows\\Web to decode");
            };
            found
        };
        eprintln!("decoding {}", path.display());
        let Some(decoded) = super::decode_wallpaper(&path) else {
            panic!("WIC could not decode {}", path.display());
        };
        eprintln!("decoded to {} bytes png {}x{}", decoded.png.len(), decoded.width, decoded.height);
        assert!(decoded.width.max(decoded.height) <= 640);
        assert!(decoded.width > 0 && decoded.height > 0);
        assert!(decoded.png.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}

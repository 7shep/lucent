//! App catalog: taskbar pins, Start pins, All apps, icons, recent files, search,
//! launching, and the signed-in user. See docs/taskbar-spec.md, module `apps`.
//!
//! Shell and COM calls run on the calling thread, which is initialised
//! apartment-threaded once (see `ensure_com`). Async commands hop to the
//! blocking pool so the async runtime never waits on the shell.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::{self, Metadata};
use std::io::ErrorKind;
use std::iter::once;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::Serialize;
use serde_json::{Map, Value};
use windows::core::{Interface, IUnknown, BSTR, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, PROPERTYKEY, SIZE};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::Security::Authentication::Identity::{GetUserNameExW, NameDisplay};
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IBindCtx, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::{
    IEnumShellItems, IShellItem, IShellItem2, IShellItemImageFactory, IShellLinkW,
    SHCreateItemFromIDList, SHCreateItemFromParsingName, SHGetKnownFolderPath, ShellExecuteExW,
    ShellLink, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads,
    BHID_EnumItems, KF_FLAG_DEFAULT, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SIGDN,
    SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, SIIGBF_BIGGERSIZEOK,
    SIIGBF_ICONONLY,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::model::{App, CatalogApp, FileEntry, SearchResults, UserInfo};

/// Category names for the Start menu's All apps list, in display order.
pub const CATEGORIES: [&str; 7] = [
    "Productivity",
    "Creativity",
    "Social",
    "Games",
    "Developer tools",
    "Utilities",
    "Other",
];

/// Keyword rules for `categorize`, first matching category wins. A keyword
/// matches when its words (split on non-alphanumerics) appear as consecutive
/// words of "<name> <aumid>", lowercased. So "git" matches "Git Bash" but not
/// "Logitech". Social comes before Creativity so Zoom's "Video" does not win.
const CATEGORY_RULES: &[(&str, &[&str])] = &[
    (
        "Productivity",
        &[
            "chrome", "edge", "firefox", "opera", "office", "word", "excel", "outlook", "notion",
            "pdf", "acrobat", "notepad",
        ],
    ),
    (
        "Social",
        &[
            "discord", "whatsapp", "teams", "slack", "telegram", "zoom", "signal",
        ],
    ),
    (
        "Creativity",
        &[
            "photo", "paint", "video", "audio", "music", "spotify", "obs", "clipchamp", "camera",
            "figma",
        ],
    ),
    (
        "Games",
        &[
            "steam", "epic", "riot", "valorant", "minecraft", "xbox", "game", "battle.net", "gog",
            "ea app", "ubisoft", "roblox",
        ],
    ),
    (
        "Developer tools",
        &[
            "code",
            "visual studio",
            "terminal",
            "powershell",
            "git",
            "python",
            "node",
            "sdk",
            "software development kit",
            "docker",
            "wsl",
            "claude",
            "chatgpt",
            "cursor",
            "jetbrains",
            "android studio",
            "intellij",
            "pycharm",
            "webstorm",
            "rider",
            "clion",
            "goland",
        ],
    ),
    (
        "Utilities",
        &[
            "settings", "control panel", "task manager", "snipping", "calculator", "7-zip",
            "everything", "nvidia", "amd", "intel", "defender", "backup", "cleanup", "tool",
        ],
    ),
];

/// Names matched exactly (case-insensitive), checked before the keyword rules.
const EXACT_NAME_RULES: &[(&str, &str)] = &[("ea", "Games")];

/// Words that mark uninstallers and documentation/web entries in All apps (lowercase).
const SKIPPED_ENTRY_WORDS: [&str; 9] = [
    "uninstall",
    "readme",
    "help",
    "documentation",
    "release notes",
    "website",
    "support center",
    "tools for desktop apps",
    "sample",
];

const TASKBAND_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Taskband";
const TASKBAND_VALUE: &str = "Favorites";
/// Upper bound on the Favorites blob (a few KB in practice).
const MAX_FAVORITES_BYTES: u32 = 1 << 20;
/// Record header: one type byte and a u32 length.
const FAVORITE_HEADER_LEN: usize = 5;
const APPSFOLDER_PREFIX: &str = r"shell:AppsFolder\";
const SEARCH_LIMIT: usize = 6;
/// Search walk limits: depth below each root and total entries visited.
const SEARCH_WALK_MAX_DEPTH: usize = 3;
const SEARCH_WALK_MAX_ENTRIES: usize = 5000;
const MAX_ICON_PX: u32 = 256;
const MAX_AVATAR_BYTES: u64 = 2 * 1024 * 1024;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// PKEY_AppUserModel_ID. Declared here because the `windows` feature that
/// exports it (Win32_Storage_EnhancedStorage) is not enabled in Cargo.toml.
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

static ALL_APPS: Mutex<Option<Vec<CatalogApp>>> = Mutex::new(None);
/// Last resolved taskbar pins, keyed by the raw Favorites bytes they came from.
static PIN_CACHE: Mutex<Option<(Vec<u8>, Vec<App>)>> = Mutex::new(None);
/// Resolved recent-file targets and when they were resolved (see `recent_snapshot`).
static RECENT_CACHE: Mutex<Option<(Instant, Vec<FileEntry>)>> = Mutex::new(None);
/// How long resolved recent targets are reused while the user types a search.
const RECENT_CACHE_TTL: Duration = Duration::from_secs(30);
static ICON_CACHE: LazyLock<Mutex<HashMap<(String, u32), Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested)
// ---------------------------------------------------------------------------

/// Splits the Taskband `Favorites` blob into the ITEMIDLIST bytes of its
/// type-0 records. Parsing stops at the first record whose length is zero or
/// overruns the blob, so a malformed tail never yields a slice past the end.
fn split_favorites(blob: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = blob;
    while rest.len() >= FAVORITE_HEADER_LEN {
        let kind = rest[0];
        let len = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
        if len == 0 || len > rest.len() - FAVORITE_HEADER_LEN {
            break;
        }
        let body = &rest[FAVORITE_HEADER_LEN..FAVORITE_HEADER_LEN + len];
        if kind == 0 {
            out.push(body);
        }
        rest = &rest[FAVORITE_HEADER_LEN + len..];
    }
    out
}

/// True when `bytes` is a complete ITEMIDLIST: at least one SHITEMID whose `cb`
/// fields all fit inside `bytes`, ending with the 2-byte zero terminator, which
/// must lie inside `bytes` too. An empty list (terminator at offset 0) is rejected.
fn is_well_formed_idlist(bytes: &[u8]) -> bool {
    let mut pos = 0usize;
    let mut items = 0usize;
    loop {
        if pos + 2 > bytes.len() {
            return false;
        }
        let cb = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        if cb == 0 {
            return items > 0;
        }
        if cb < 2 || pos + cb > bytes.len() {
            return false;
        }
        items += 1;
        pos += cb;
    }
}

/// Lowercase words of `text`, split on anything that is not a letter or digit.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// True when the keyword's words appear as consecutive words of `haystack`.
fn has_word_phrase(haystack: &[String], keyword: &str) -> bool {
    let phrase = words(keyword);
    !phrase.is_empty() && haystack.windows(phrase.len()).any(|w| w == phrase.as_slice())
}

/// Category for an app from its name and AUMID. Exact names are checked first,
/// then the keyword rules in table order; the first match wins.
fn categorize(name: &str, aumid: &str) -> &'static str {
    let exact = name.trim().to_lowercase();
    if let Some((_, category)) = EXACT_NAME_RULES.iter().find(|(n, _)| exact == *n) {
        return category;
    }
    let haystack = words(&format!("{name} {aumid}"));
    CATEGORY_RULES
        .iter()
        .find(|(_, keywords)| keywords.iter().any(|k| has_word_phrase(&haystack, k)))
        .map(|(category, _)| *category)
        .unwrap_or("Other")
}

/// True for uninstallers and documentation entries that stay out of All apps.
fn is_skipped_entry(name: &str) -> bool {
    let lower = name.to_lowercase();
    SKIPPED_ENTRY_WORDS.iter().any(|word| lower.contains(word))
}

/// True when a target is a web address (http or https).
fn is_web_target(target: &str) -> bool {
    let t = target.trim().to_ascii_lowercase();
    t.starts_with("http://") || t.starts_with("https://")
}

/// True when a file path is a `.url` internet shortcut.
fn is_url_file(path: &str) -> bool {
    path.trim().to_ascii_lowercase().ends_with(".url")
}

/// True when an All apps entry is documentation or a web link, which the
/// catalog leaves out. `file_path` is the item's file path (if it has one) and
/// `lnk_target` is the resolved target of a .lnk (if any).
fn skip_catalog_entry(name: &str, file_path: Option<&str>, lnk_target: Option<&str>) -> bool {
    is_skipped_entry(name)
        || file_path.is_some_and(is_url_file)
        || lnk_target.is_some_and(is_web_target)
}

/// Sorts All apps by name, drops skipped entries and entries without an id,
/// and keeps one entry per id (case-insensitive).
fn finish_catalog(items: Vec<CatalogApp>) -> Vec<CatalogApp> {
    let mut items: Vec<CatalogApp> = items
        .into_iter()
        .filter(|c| !c.app.id.trim().is_empty() && !is_skipped_entry(&c.app.name))
        .collect();
    items.sort_by(|a, b| {
        a.app
            .name
            .to_lowercase()
            .cmp(&b.app.name.to_lowercase())
            .then_with(|| a.app.id.cmp(&b.app.id))
    });
    let mut seen = HashSet::new();
    items.retain(|c| seen.insert(c.app.id.to_lowercase()));
    items
}

/// Name matches for `query` (trimmed, case-insensitive). Prefix matches come
/// first; each group keeps its input order. At most `limit` results.
fn rank_by_name<T, I, F>(items: I, query: &str, name_of: F, limit: usize) -> Vec<T>
where
    I: IntoIterator<Item = T>,
    F: Fn(&T) -> &str,
{
    let needle = query.trim().to_lowercase();
    if needle.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut prefix = Vec::new();
    let mut contains = Vec::new();
    for item in items {
        let lower = name_of(&item).to_lowercase();
        if lower.starts_with(&needle) {
            prefix.push(item);
        } else if lower.contains(&needle) {
            contains.push(item);
        }
    }
    prefix.into_iter().chain(contains).take(limit).collect()
}

/// Sorts newest first (ties by path), keeps the first entry per path
/// (case-insensitive), and truncates to `limit`.
fn finish_recent(mut entries: Vec<FileEntry>, limit: usize) -> Vec<FileEntry> {
    entries.sort_by(|a, b| {
        b.modified_ms
            .cmp(&a.modified_ms)
            .then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase()))
    });
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter(|e| seen.insert(e.path.to_lowercase()))
        .take(limit)
        .collect()
}

/// True when a cached value stored at `stored_at` is still usable at `now`:
/// it exists and is younger than `ttl`. No stored value means not fresh.
fn cache_is_fresh(stored_at: Option<Instant>, now: Instant, ttl: Duration) -> bool {
    stored_at.is_some_and(|at| now.saturating_duration_since(at) < ttl)
}

/// Number of path components of `path` below `root`. None when `path` is not under `root`.
fn relative_depth(root: &Path, path: &Path) -> Option<usize> {
    path.strip_prefix(root)
        .ok()
        .map(|rel| rel.components().count())
}

/// Recent-file targets must be plain absolute file paths (no URIs such as ms-settings:).
fn is_plain_path_target(target: &str) -> bool {
    let t = target.trim();
    !t.is_empty()
        && !t.contains("://")
        && !t.to_ascii_lowercase().starts_with("ms-")
        && Path::new(t).is_absolute()
}

/// Adds (no duplicate ids) or removes (by id) `app` from the Start pins.
fn apply_start_pin(pins: &[App], app: &App, pinned: bool) -> Vec<App> {
    let same = |p: &App| p.id.eq_ignore_ascii_case(&app.id);
    if pinned {
        let mut out = dedupe_by_id(pins.to_vec());
        if !out.iter().any(|p| same(p)) {
            out.push(app.clone());
        }
        out
    } else {
        pins.iter().filter(|p| !same(p)).cloned().collect()
    }
}

/// Keeps the first app for each id (case-insensitive).
fn dedupe_by_id(apps: Vec<App>) -> Vec<App> {
    let mut seen = HashSet::new();
    apps.into_iter()
        .filter(|a| seen.insert(a.id.to_lowercase()))
        .collect()
}

/// Shape check for launch targets: `shell:AppsFolder\<AUMID>` or an absolute path.
fn is_launch_target(target: &str) -> bool {
    match appsfolder_aumid(target) {
        Some(aumid) => !aumid.is_empty() && !aumid.contains('\0'),
        None => Path::new(target).is_absolute(),
    }
}

/// The AUMID part of a `shell:AppsFolder\<AUMID>` target.
fn appsfolder_aumid(target: &str) -> Option<&str> {
    let head = target.get(..APPSFOLDER_PREFIX.len())?;
    if head.eq_ignore_ascii_case(APPSFOLDER_PREFIX) {
        target.get(APPSFOLDER_PREFIX.len()..)
    } else {
        None
    }
}

/// `shell:AppsFolder\<AUMID>` for a packaged app.
fn appsfolder_launch(aumid: &str) -> String {
    format!("{APPSFOLDER_PREFIX}{aumid}")
}

/// Removes a trailing ".lnk" (case-insensitive). Leaves other names alone.
fn strip_lnk_suffix(name: &str) -> &str {
    let n = name.len();
    if n >= 4 && name.is_char_boundary(n - 4) && name[n - 4..].eq_ignore_ascii_case(".lnk") {
        &name[..n - 4]
    } else {
        name
    }
}

/// Directory names skipped by the search walk: dot-directories and node_modules.
fn is_skipped_dir_name(name: &str) -> bool {
    name.starts_with('.') || name.eq_ignore_ascii_case("node_modules")
}

/// True when file attributes mark an entry the search walk must skip:
/// hidden, system, or a reparse point (symlink or junction).
fn skip_attributes(attributes: u32) -> bool {
    attributes & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_REPARSE_POINT)
        != 0
}

/// Image MIME type for avatar and icon files by extension.
fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// Percent-encodes `s` as UTF-8: unreserved characters stay, everything else
/// (including spaces) becomes %XX.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Bing search URL for `query`.
fn search_url(query: &str) -> String {
    format!("https://www.bing.com/search?q={}", url_encode(query))
}

/// Formats a binary SID (revision, sub-authority count, 6-byte authority,
/// u32 LE sub-authorities) as `S-1-...`. None when the bytes are too short.
fn sid_to_string(sid: &[u8]) -> Option<String> {
    let (&revision, rest) = sid.split_first()?;
    let (&count, rest) = rest.split_first()?;
    let count = usize::from(count);
    if rest.len() < 6 + 4 * count {
        return None;
    }
    let authority = rest[..6]
        .iter()
        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
    let mut out = format!("S-{revision}-{authority}");
    for chunk in rest[6..6 + 4 * count].chunks_exact(4) {
        let value = u32::from_le_bytes(<[u8; 4]>::try_from(chunk).ok()?);
        out.push_str(&format!("-{value}"));
    }
    Some(out)
}

/// Converts a top-down BGRA buffer from GetDIBits (premultiplied alpha) to
/// straight RGBA. When every alpha byte is zero the icon has no alpha channel,
/// so it is treated as opaque.
fn bgra_premul_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let has_alpha = bgra.chunks_exact(4).any(|px| px[3] != 0);
    let mut out = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        if !has_alpha {
            out.extend_from_slice(&[r, g, b, 255]);
        } else if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let unpremultiply = |c: u8| -> u8 {
                let a = u32::from(a);
                ((u32::from(c) * 255 + a / 2) / a).min(255) as u8
            };
            out.extend_from_slice(&[unpremultiply(r), unpremultiply(g), unpremultiply(b), a]);
        }
    }
    out
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

/// Serde value for a serialisable probe field. Falls back to null.
fn to_json<T: Serialize + ?Sized>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Parses `startPins` from config.json text. Ok(None) when the key is absent.
fn parse_start_pins(text: &str) -> Result<Option<Vec<App>>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Some(list) = value.get("startPins") else {
        return Ok(None);
    };
    let apps: Vec<App> = serde_json::from_value(list.clone()).map_err(|e| e.to_string())?;
    Ok(Some(dedupe_by_id(apps)))
}

/// Files under `root` whose name contains `needle` (lowercase), appended to `out`.
/// Visits at most `budget` entries in total, descends at most
/// SEARCH_WALK_MAX_DEPTH levels, and skips hidden, system, reparse and
/// dot/node_modules directories.
fn walk_for_search(root: &Path, needle: &str, budget: &mut usize, out: &mut Vec<FileEntry>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            let path = entry.path();
            let Some(depth) = relative_depth(root, &path) else {
                continue;
            };
            if depth == 0 || depth > SEARCH_WALK_MAX_DEPTH {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if skip_attributes(meta.file_attributes()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if meta.is_dir() {
                if depth < SEARCH_WALK_MAX_DEPTH && !is_skipped_dir_name(&name) {
                    stack.push(path);
                }
            } else if meta.is_file() && name.to_lowercase().contains(needle) {
                out.push(FileEntry {
                    name,
                    path: path.to_string_lossy().into_owned(),
                    modified_ms: meta.modified().map(system_time_ms).unwrap_or(0),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Small Windows helpers
// ---------------------------------------------------------------------------

/// Locks a mutex, recovering the data if another thread panicked while holding it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Initialises COM as apartment-threaded on the calling thread, once per thread.
/// S_FALSE and RPC_E_CHANGED_MODE are both fine: the thread already has a mode
/// and COM calls still work. The apartment is never torn down, since pool
/// threads are reused.
fn ensure_com() {
    COM_READY.with(|ready| {
        if !ready.get() {
            // SAFETY: CoInitializeEx with a null pvReserved has no preconditions
            // beyond running on this thread. The HRESULT is ignored on purpose.
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
        Some(s.encode_utf16().chain(once(0)).collect())
    }
}

/// Wide, NUL-terminated copy of a known-good literal.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

fn system_time_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

fn modified_ms(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .map(system_time_ms)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `.lnk` files directly inside `dir`. Empty when the folder cannot be read.
fn lnk_files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")))
        .collect()
}

/// Reads a COM-allocated wide string, frees it, and returns it (None when empty or invalid).
///
/// # Safety
/// `ptr` must be a string allocated with the COM task allocator that the caller owns.
unsafe fn take_pwstr(ptr: PWSTR) -> Option<String> {
    if ptr.0.is_null() {
        return None;
    }
    // SAFETY: the pointer is non-null and NUL-terminated (the caller owns it).
    let text = unsafe { ptr.to_string() }.ok();
    // SAFETY: the caller owns this COM allocation and it is freed exactly once here.
    unsafe { CoTaskMemFree(Some(ptr.0 as *const c_void)) };
    text.filter(|s| !s.is_empty())
}

/// Display string of a shell item in the given form, or None.
fn display_name(item: &IShellItem, form: SIGDN) -> Option<String> {
    // SAFETY: `item` is a live shell item. On success the returned string is
    // owned by us and handed to take_pwstr, which frees it.
    let ptr = unsafe { item.GetDisplayName(form) }.ok()?;
    // SAFETY: `ptr` is a COM allocation we now own.
    unsafe { take_pwstr(ptr) }
}

/// Shell item for a parsing name (file path, .lnk, or `shell:AppsFolder\...`).
fn shell_item_for_parsing(path: &str) -> Option<IShellItem> {
    ensure_com();
    let wide_path = wide_checked(path)?;
    // SAFETY: `wide_path` is NUL-terminated and lives across the call. A null
    // bind context is allowed.
    unsafe {
        SHCreateItemFromParsingName(PCWSTR(wide_path.as_ptr()), None::<&IBindCtx>)
    }
    .ok()
}

/// AUMID stored on a shell item (`System.AppUserModel.ID`), if any.
fn aumid_of(item: &IShellItem) -> Option<String> {
    let item2: IShellItem2 = item.cast().ok()?;
    // SAFETY: the key is a valid static PROPERTYKEY; the returned PROPVARIANT is
    // owned and cleared when dropped.
    let value = unsafe { item2.GetProperty(&PKEY_APP_USER_MODEL_ID) }.ok()?;
    if value.is_empty() {
        return None;
    }
    BSTR::try_from(&value)
        .ok()
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

/// Target path of a .lnk file. Resolves nothing interactively: no UI, no search.
fn lnk_target(lnk: &Path) -> Option<String> {
    ensure_com();
    let wide_path = wide_checked(lnk.to_str()?)?;
    // SAFETY: CLSID_ShellLink is a valid class id; the returned interface is owned.
    let link: IShellLinkW =
        unsafe { CoCreateInstance(&ShellLink, None::<&IUnknown>, CLSCTX_INPROC_SERVER) }.ok()?;
    let persist: IPersistFile = link.cast().ok()?;
    // SAFETY: `wide_path` is NUL-terminated and lives across the call.
    unsafe { persist.Load(PCWSTR(wide_path.as_ptr()), STGM_READ) }.ok()?;
    let mut buf = vec![0u16; 32768];
    // SAFETY: `buf` is a writable slice of the length passed; a null find-data
    // pointer is allowed.
    unsafe { link.GetPath(&mut buf, std::ptr::null_mut(), 0) }.ok()?;
    let len = buf.iter().position(|&c| c == 0)?;
    String::from_utf16(&buf[..len]).ok().filter(|s| !s.is_empty())
}

/// Known folder path for the current user, or None.
fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: `id` points at a static known-folder GUID. A null token means the
    // current user. The returned string is owned and freed by take_pwstr.
    let ptr = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: `ptr` is a COM allocation we now own.
    unsafe { take_pwstr(ptr) }.map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Taskbar pins
// ---------------------------------------------------------------------------

/// Reads the raw Taskband `Favorites` value. None when missing or unreadable.
fn read_favorites() -> Option<Vec<u8>> {
    let key = wide(TASKBAND_KEY);
    let value = wide(TASKBAND_VALUE);
    let mut size: u32 = 0;
    // SAFETY: `key` and `value` are NUL-terminated and live across both calls.
    // With no data buffer, RegGetValueW only writes the required size.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            None,
            Some(&mut size as *mut u32),
        )
    };
    if status.is_err() || size == 0 || size > MAX_FAVORITES_BYTES {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let mut len = size;
    // SAFETY: `buf` has `size` writable bytes; `len` starts at that capacity and
    // the call writes no more than it.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len as *mut u32),
        )
    };
    let len = usize::try_from(len).ok()?;
    if status.is_err() || len == 0 || len > buf.len() {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

/// App for one ITEMIDLIST from the Favorites blob. None when the record is
/// malformed or the shell cannot resolve it.
fn app_from_idlist(record: &[u8]) -> Option<App> {
    if !is_well_formed_idlist(record) {
        return None;
    }
    ensure_com();
    // ITEMIDLIST needs 2-byte alignment and the record may start at an odd
    // offset, so copy it into a u16 buffer (little-endian, as on Windows).
    let mut aligned = vec![0u16; record.len().div_ceil(2)];
    for (slot, pair) in aligned.iter_mut().zip(record.chunks(2)) {
        *slot = u16::from_le_bytes([pair[0], pair.get(1).copied().unwrap_or(0)]);
    }
    let pidl = aligned.as_ptr().cast::<ITEMIDLIST>();
    // SAFETY: `aligned` holds the record (2-byte aligned). is_well_formed_idlist
    // proved a zero terminator lies inside the record, so the shell stops within
    // those bytes. The buffer outlives the call.
    let item: IShellItem = unsafe { SHCreateItemFromIDList(pidl) }.ok()?;
    match display_name(&item, SIGDN_FILESYSPATH) {
        Some(path) => app_from_lnk(&item, &path),
        None => {
            // Packaged app: the parent-relative parsing name of an AppsFolder child is its AUMID.
            let aumid = display_name(&item, SIGDN_PARENTRELATIVEPARSING)?;
            let name = display_name(&item, SIGDN_NORMALDISPLAY)?;
            Some(App {
                id: aumid.clone(),
                name,
                launch: appsfolder_launch(&aumid),
            })
        }
    }
}

/// App for a .lnk file shell item. The id is the AUMID when set, else the
/// lowercase target path (else the lowercase .lnk path when no target resolves).
fn app_from_lnk(item: &IShellItem, lnk_path: &str) -> Option<App> {
    let id = match aumid_of(item) {
        Some(aumid) => aumid,
        None => match lnk_target(Path::new(lnk_path)) {
            Some(target) => target.to_lowercase(),
            None => lnk_path.to_lowercase(),
        },
    };
    let display = display_name(item, SIGDN_NORMALDISPLAY).or_else(|| {
        Path::new(lnk_path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
    })?;
    let name = strip_lnk_suffix(&display).to_string();
    if name.trim().is_empty() {
        return None;
    }
    Some(App {
        id,
        name,
        launch: lnk_path.to_string(),
    })
}

/// App for a .lnk file path (used by the Quick Launch fallback).
fn app_from_lnk_path(path: &Path) -> Option<App> {
    let text = path.to_str()?;
    let item = shell_item_for_parsing(text)?;
    app_from_lnk(&item, text)
}

/// Fallback pins: .lnk files in the Quick Launch TaskBar folder, sorted by name.
fn quick_launch_pins() -> Vec<App> {
    let Some(appdata) = env_path("APPDATA") else {
        return Vec::new();
    };
    let dir = appdata.join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar");
    let mut lnks = lnk_files_in(&dir);
    lnks.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    lnks.iter().filter_map(|p| app_from_lnk_path(p)).collect()
}

/// Returns the cached value when `blob` equals the bytes it was resolved from.
/// Otherwise runs `resolve`, stores the result with `blob`, and returns it.
/// The bool is true when `resolve` ran.
fn cached_or_resolve<T: Clone>(
    cache: &mut Option<(Vec<u8>, T)>,
    blob: &[u8],
    resolve: impl FnOnce() -> T,
) -> (T, bool) {
    if let Some((cached_blob, value)) = cache.as_ref() {
        if cached_blob.as_slice() == blob {
            return (value.clone(), false);
        }
    }
    let value = resolve();
    *cache = Some((blob.to_vec(), value.clone()));
    (value, true)
}

/// Taskbar pins in the user's order, read from Windows' own pin list.
///
/// Cheap on repeat calls: only the registry value is read. The shell items are
/// resolved again only when the Favorites bytes change. When the value cannot be
/// read or resolves to nothing, the Quick Launch fallback runs and is not cached.
pub fn pinned_apps() -> Vec<App> {
    ensure_com();
    let Some(blob) = read_favorites() else {
        return quick_launch_pins();
    };
    let resolved = {
        let mut cache = lock(&PIN_CACHE);
        cached_or_resolve(&mut cache, &blob, || {
            let pins: Vec<App> = split_favorites(&blob)
                .into_iter()
                .filter_map(app_from_idlist)
                .collect();
            dedupe_by_id(pins)
        })
        .0
    };
    if resolved.is_empty() {
        quick_launch_pins()
    } else {
        resolved
    }
}

// ---------------------------------------------------------------------------
// All apps
// ---------------------------------------------------------------------------

/// Catalog entry for one AppsFolder child.
fn catalog_entry(item: &IShellItem) -> Option<CatalogApp> {
    let app = match display_name(item, SIGDN_FILESYSPATH) {
        // Desktop app shortcut in the Start menu. Documentation and web links are left out.
        Some(path) => {
            let name = display_name(item, SIGDN_NORMALDISPLAY)?;
            let target = lnk_target(Path::new(&path));
            if skip_catalog_entry(&name, Some(&path), target.as_deref()) {
                return None;
            }
            app_from_lnk(item, &path)?
        }
        None => {
            let aumid = display_name(item, SIGDN_PARENTRELATIVEPARSING)?;
            let name = display_name(item, SIGDN_NORMALDISPLAY)?;
            if skip_catalog_entry(&name, None, None) {
                return None;
            }
            App {
                id: aumid.clone(),
                name,
                launch: appsfolder_launch(&aumid),
            }
        }
    };
    let category = categorize(&app.name, &app.id);
    Some(CatalogApp {
        app,
        category: category.to_string(),
    })
}

/// Enumerates shell:AppsFolder once (the result is cached by `all_apps`).
fn enumerate_all_apps() -> Vec<CatalogApp> {
    ensure_com();
    let Some(folder) = shell_item_for_parsing("shell:AppsFolder") else {
        return Vec::new();
    };
    // SAFETY: `folder` is a live shell item. BHID_EnumItems is a valid handler id.
    let enumerator: IEnumShellItems =
        match unsafe { folder.BindToHandler(None::<&IBindCtx>, &BHID_EnumItems) } {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
    let mut out = Vec::new();
    loop {
        let mut batch: [Option<IShellItem>; 16] = Default::default();
        let mut fetched: u32 = 0;
        // SAFETY: `batch` has 16 slots and `fetched` receives the count written.
        let result = unsafe { enumerator.Next(&mut batch, Some(&mut fetched as *mut u32)) };
        if result.is_err() || fetched == 0 {
            break;
        }
        for item in batch.iter().take(fetched as usize).flatten() {
            if let Some(entry) = catalog_entry(item) {
                out.push(entry);
            }
        }
    }
    finish_catalog(out)
}

/// All apps, cached after the first successful enumeration.
fn all_apps() -> Vec<CatalogApp> {
    if let Some(cached) = lock(&ALL_APPS).as_ref() {
        return cached.clone();
    }
    let apps = enumerate_all_apps();
    if !apps.is_empty() {
        *lock(&ALL_APPS) = Some(apps.clone());
    }
    apps
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// Copies the pixels of `hbmp` as top-down BGRA. Returns (width, height, bytes).
/// The caller still owns `hbmp`.
fn read_bitmap_bgra(hbmp: HBITMAP, fallback: u32) -> Option<(u32, u32, Vec<u8>)> {
    let mut info = BITMAP::default();
    // SAFETY: `info` is a BITMAP-sized writable struct, and `hbmp` is a valid
    // bitmap handle for the duration of the call.
    let copied = unsafe {
        GetObjectW(
            hbmp.into(),
            i32::try_from(std::mem::size_of::<BITMAP>()).ok()?,
            Some(&mut info as *mut BITMAP as *mut c_void),
        )
    };
    let (w, h) = if copied > 0 && info.bmWidth > 0 && info.bmHeight > 0 {
        (info.bmWidth as u32, info.bmHeight as u32)
    } else {
        (fallback, fallback)
    };
    if w == 0 || h == 0 || w > MAX_ICON_PX || h > MAX_ICON_PX {
        return None;
    }
    let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        biHeight: -(h as i32), // negative: top-down rows
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    // SAFETY: GetDC(None) returns the screen DC, released below on every path.
    let hdc = unsafe { GetDC(None) };
    if hdc.is_invalid() {
        return None;
    }
    // SAFETY: `bgra` holds w*h*4 bytes, matching the 32-bit top-down DIB that
    // `bmi` describes. `hbmp` and `hdc` are valid for the call.
    let lines = unsafe {
        GetDIBits(
            hdc,
            hbmp,
            0,
            h,
            Some(bgra.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: `hdc` came from GetDC(None) above and is released exactly once.
    unsafe { ReleaseDC(None, hdc) };
    if lines != h as i32 {
        return None;
    }
    Some((w, h, bgra))
}

/// Renders the icon of `launch` at `size` px as a PNG data URL.
fn render_icon(launch: &str, size: u32) -> Option<String> {
    let item = shell_item_for_parsing(launch)?;
    let factory: IShellItemImageFactory = item.cast().ok()?;
    let px = i32::try_from(size).ok()?;
    // SAFETY: `factory` is a live COM interface and SIZE is passed by value.
    let hbmp = unsafe {
        factory.GetImage(
            SIZE { cx: px, cy: px },
            SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
        )
    }
    .ok()?;
    // GetImage transfers ownership of hbmp to us. It is deleted below on every path.
    let png = read_bitmap_bgra(hbmp, size)
        .and_then(|(w, h, bgra)| encode_png(w, h, &bgra_premul_to_rgba(&bgra)));
    // SAFETY: `hbmp` is owned by us and deleted exactly once.
    let _ = unsafe { DeleteObject(hbmp.into()) };
    let png = png?;
    Some(format!("data:image/png;base64,{}", STANDARD.encode(png)))
}

/// `render_icon` with the (launch, size) cache.
fn cached_icon(launch: &str, size: u32) -> Option<String> {
    let key = (launch.to_string(), size);
    if let Some(hit) = lock(&ICON_CACHE).get(&key) {
        return hit.clone();
    }
    ensure_com();
    let icon = render_icon(launch, size);
    lock(&ICON_CACHE).insert(key, icon.clone());
    icon
}

// ---------------------------------------------------------------------------
// Recent files and search
// ---------------------------------------------------------------------------

/// Recent files: targets of the .lnk files in the Recent folder, newest first,
/// deduplicated, existing plain files only.
fn recent_files(limit: usize) -> Vec<FileEntry> {
    ensure_com();
    let Some(appdata) = env_path("APPDATA") else {
        return Vec::new();
    };
    let dir = appdata.join(r"Microsoft\Windows\Recent");
    let mut candidates: Vec<(PathBuf, i64)> = lnk_files_in(&dir)
        .into_iter()
        .filter_map(|p| {
            let ms = modified_ms(&p)?;
            Some((p, ms))
        })
        .collect();
    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    let entries: Vec<FileEntry> = candidates
        .into_iter()
        .filter_map(|(lnk, ms)| {
            let target = lnk_target(&lnk)?;
            if !is_plain_path_target(&target) || !Path::new(&target).is_file() {
                return None;
            }
            let name = Path::new(&target)
                .file_name()?
                .to_string_lossy()
                .into_owned();
            Some(FileEntry {
                name,
                path: target,
                modified_ms: ms,
            })
        })
        .collect();
    finish_recent(entries, limit)
}

/// Recent targets for search. Reuses the last resolution for RECENT_CACHE_TTL,
/// so typing does not re-resolve every Recent .lnk on each keystroke.
fn recent_snapshot() -> Vec<FileEntry> {
    let now = Instant::now();
    if let Some((at, entries)) = lock(&RECENT_CACHE).as_ref() {
        if cache_is_fresh(Some(*at), now, RECENT_CACHE_TTL) {
            return entries.clone();
        }
    }
    let entries = recent_files(usize::MAX);
    *lock(&RECENT_CACHE) = Some((now, entries.clone()));
    entries
}

/// Folders searched for files: Desktop, Documents and Downloads.
fn search_roots() -> Vec<PathBuf> {
    [&FOLDERID_Desktop, &FOLDERID_Documents, &FOLDERID_Downloads]
        .into_iter()
        .filter_map(known_folder)
        .collect()
}

/// Files whose name contains `query`: recent targets plus a bounded walk of
/// the search roots. Newest first, at most SEARCH_LIMIT.
fn search_files(query: &str) -> Vec<FileEntry> {
    let needle = query.trim().to_lowercase();
    let mut found: Vec<FileEntry> = recent_snapshot()
        .into_iter()
        .filter(|f| f.name.to_lowercase().contains(&needle))
        .collect();
    let mut budget = SEARCH_WALK_MAX_ENTRIES;
    for root in search_roots() {
        walk_for_search(&root, &needle, &mut budget, &mut found);
    }
    finish_recent(found, SEARCH_LIMIT)
}

fn search_now(query: &str) -> SearchResults {
    let q = query.trim();
    if q.is_empty() {
        return SearchResults::default();
    }
    let apps = rank_by_name(
        all_apps().into_iter().map(|c| c.app),
        q,
        |a: &App| a.name.as_str(),
        SEARCH_LIMIT,
    );
    SearchResults {
        apps,
        files: search_files(q),
    }
}

// ---------------------------------------------------------------------------
// Launching
// ---------------------------------------------------------------------------

/// ShellExecuteExW with verb "open" on `target`. Initialises COM on the calling
/// thread first, so it is safe on any blocking-pool worker.
fn shell_open(target: &str) -> Result<(), String> {
    ensure_com();
    let verb = wide("open");
    let file = wide_checked(target).ok_or_else(|| "target contains a NUL character".to_string())?;
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` is fully initialised. `verb` and `file` are NUL-terminated
    // and outlive the call. ShellExecuteExW only reads them.
    unsafe { ShellExecuteExW(&mut info) }.map_err(|e| format!("could not open {target}: {e}"))
}

// ---------------------------------------------------------------------------
// User
// ---------------------------------------------------------------------------

/// Owns a token handle and closes it on drop.
struct OwnedToken(HANDLE);

impl Drop for OwnedToken {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcessToken and is closed exactly once here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// SID string of the current user, e.g. `S-1-5-21-...`.
fn current_user_sid() -> Option<String> {
    let mut raw = HANDLE(std::ptr::null_mut());
    // SAFETY: GetCurrentProcess is a pseudo-handle and needs no closing. `raw`
    // receives a real token handle, which OwnedToken closes.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }.ok()?;
    let token = OwnedToken(raw);
    let mut needed: u32 = 0;
    // SAFETY: a null buffer with length 0 only asks for the required size.
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut needed) };
    if needed == 0 {
        return None;
    }
    // u64 storage keeps the buffer 8-byte aligned, as TOKEN_USER requires.
    let mut buf = vec![0u64; usize::try_from(needed).ok()?.div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` writable bytes, 8-byte aligned.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .ok()?;
    // SAFETY: a successful TokenUser query writes a TOKEN_USER at the start of
    // `buf`. Its SID pointer refers to memory inside `buf`, which lives to the
    // end of this function.
    let user = unsafe { &*buf.as_ptr().cast::<TOKEN_USER>() };
    let sid = user.User.Sid.0.cast::<u8>();
    // SAFETY: a SID is revision (1 byte), sub-authority count (1 byte), a 6-byte
    // authority, then `count` u32 sub-authorities. The first two bytes are inside `buf`.
    let count = usize::from(unsafe { *sid.add(1) });
    // SAFETY: the whole SID (8 + 4*count bytes) lies inside `buf` for the same reason.
    let bytes = unsafe { std::slice::from_raw_parts(sid, 8 + 4 * count) };
    sid_to_string(bytes)
}

/// Display name of the signed-in user, falling back to %USERNAME%.
fn display_user_name() -> String {
    let mut size: u32 = 0;
    // SAFETY: a null buffer asks for the required length, which is written to `size`.
    unsafe { GetUserNameExW(NameDisplay, None, &mut size) };
    if size > 0 {
        let mut buf = vec![0u16; size as usize];
        // SAFETY: `buf` has `size` UTF-16 slots, and `size` is updated to the count written.
        let ok = unsafe { GetUserNameExW(NameDisplay, Some(PWSTR(buf.as_mut_ptr())), &mut size) };
        if ok {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let name = String::from_utf16_lossy(&buf[..len]);
            if !name.trim().is_empty() {
                return name;
            }
        }
    }
    std::env::var("USERNAME").unwrap_or_default()
}

/// Largest or newest image file in `dir`, chosen by `better` (higher wins).
fn best_image(dir: &Path, better: impl Fn(&Metadata) -> u64) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| image_mime(&e.path()).is_some())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| (better(&meta), e.path()))
        })
        .max_by_key(|(key, _)| *key)
        .map(|(_, path)| path)
}

/// Account picture as a data URL: newest image in %APPDATA%\...\AccountPictures,
/// else the largest image in Public\AccountPictures\<SID>.
fn user_avatar() -> Option<String> {
    let newest = env_path("APPDATA")
        .map(|p| p.join(r"Microsoft\Windows\AccountPictures"))
        .and_then(|dir| {
            best_image(&dir, |m| {
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as u64)
            })
        });
    let file = newest.or_else(|| {
        let sid = current_user_sid()?;
        let public = env_path("PUBLIC").unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"));
        best_image(&public.join("AccountPictures").join(sid), |m| m.len())
    })?;
    let mime = image_mime(&file)?;
    if fs::metadata(&file).ok()?.len() > MAX_AVATAR_BYTES {
        return None;
    }
    let bytes = fs::read(&file).ok()?;
    Some(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

fn user_info() -> UserInfo {
    UserInfo {
        name: display_user_name(),
        avatar: user_avatar(),
    }
}

// ---------------------------------------------------------------------------
// Start pins (Lucent's own list in %APPDATA%\Lucent\config.json)
// ---------------------------------------------------------------------------

fn start_pins_path() -> Option<PathBuf> {
    env_path("APPDATA").map(|p| p.join("Lucent").join("config.json"))
}

/// Reads `startPins` from config.json. Ok(None) when the file or the key is absent.
fn read_start_pins() -> Result<Option<Vec<App>>, String> {
    let path = start_pins_path().ok_or_else(|| "APPDATA is not set".to_string())?;
    match fs::read_to_string(&path) {
        Ok(text) => parse_start_pins(&text),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Writes `startPins` into config.json, keeping other keys. Writes through a
/// temp file so a crash cannot leave half a file.
fn save_start_pins(pins: &[App]) -> Result<(), String> {
    let path = start_pins_path().ok_or_else(|| "APPDATA is not set".to_string())?;
    let mut root = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?,
        Err(e) if e.kind() == ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e.to_string()),
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| "config.json is not a JSON object; not overwriting it".to_string())?;
    object.insert(
        "startPins".to_string(),
        serde_json::to_value(pins).map_err(|e| e.to_string())?,
    );
    let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Start pins. On first run (no saved list) they are seeded from the taskbar pins.
/// `seed` = false leaves the file alone (used by the probe).
fn load_start_pins(seed: bool) -> Result<Vec<App>, String> {
    if let Some(pins) = read_start_pins()? {
        return Ok(pins);
    }
    let seeded = pinned_apps();
    if seed {
        if let Err(e) = save_start_pins(&seeded) {
            eprintln!("lucent: could not save start pins: {e}");
        }
    }
    Ok(seeded)
}

// ---------------------------------------------------------------------------
// Probe (`lucent --print-apps`)
// ---------------------------------------------------------------------------

/// `lucent --print-apps`: pins, Start pins, categories, recent files as JSON.
/// Read-only: it does not seed config.json.
pub fn print_probe() {
    let pinned = pinned_apps();
    let start_pins = read_start_pins()
        .ok()
        .flatten()
        .unwrap_or_else(|| pinned.clone());
    let catalog = all_apps();
    let mut categories = Map::new();
    for category in CATEGORIES {
        let count = catalog.iter().filter(|c| c.category == category).count();
        categories.insert(category.to_string(), Value::from(count));
    }
    let mut root = Map::new();
    root.insert("pinned".into(), to_json(&pinned));
    root.insert("startPins".into(), to_json(&start_pins));
    root.insert("allAppsCount".into(), Value::from(catalog.len()));
    root.insert("categories".into(), Value::Object(categories));
    root.insert("recent".into(), to_json(&recent_files(6)));
    match serde_json::to_string_pretty(&Value::Object(root)) {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("lucent: could not print apps probe: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Tauri commands (names and signatures are a contract with the UI)
// ---------------------------------------------------------------------------

/// Runs blocking shell, COM, and file work on the blocking pool.
async fn blocking<T, F>(work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work).await.ok()
}

/// `blocking` for work that reports an error: a pool failure becomes an Err too.
async fn blocking_result<F>(work: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| format!("background task failed: {e}"))?
}

#[tauri::command]
pub fn get_pinned() -> Vec<App> {
    pinned_apps()
}

#[tauri::command]
pub async fn get_all_apps() -> Vec<CatalogApp> {
    blocking(all_apps).await.unwrap_or_default()
}

#[tauri::command]
pub async fn get_icon(launch: String, size: u32) -> Option<String> {
    let size = size.clamp(1, MAX_ICON_PX);
    blocking(move || cached_icon(&launch, size)).await.flatten()
}

#[tauri::command]
pub async fn get_recent(limit: usize) -> Vec<FileEntry> {
    blocking(move || recent_files(limit)).await.unwrap_or_default()
}

#[tauri::command]
pub async fn search(query: String) -> SearchResults {
    blocking(move || search_now(&query)).await.unwrap_or_default()
}

/// Validates `launch` and opens it. Runs on a blocking-pool worker.
fn launch_now(launch: &str) -> Result<(), String> {
    if !is_launch_target(launch) {
        return Err(format!("not a launch target: {launch}"));
    }
    if appsfolder_aumid(launch).is_none() && !Path::new(launch).exists() {
        return Err(format!("{launch} does not exist"));
    }
    shell_open(launch)
}

/// Validates `path` and opens it. Runs on a blocking-pool worker.
fn open_path_now(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(format!("not an absolute path: {path}"));
    }
    if !p.exists() {
        return Err(format!("{path} does not exist"));
    }
    shell_open(path)
}

/// Validates `query` and opens the Bing search for it. Runs on a blocking-pool worker.
fn web_search_now(query: &str) -> Result<(), String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("empty search".into());
    }
    shell_open(&search_url(q))
}

#[tauri::command]
pub async fn launch(launch: String) -> Result<(), String> {
    blocking_result(move || launch_now(&launch)).await
}

#[tauri::command]
pub async fn open_path(path: String) -> Result<(), String> {
    blocking_result(move || open_path_now(&path)).await
}

#[tauri::command]
pub async fn web_search(query: String) -> Result<(), String> {
    blocking_result(move || web_search_now(&query)).await
}

#[tauri::command]
pub async fn get_user() -> UserInfo {
    blocking(user_info).await.unwrap_or_else(|| UserInfo {
        name: std::env::var("USERNAME").unwrap_or_default(),
        avatar: None,
    })
}

#[tauri::command]
pub fn get_start_pins() -> Vec<App> {
    load_start_pins(true).unwrap_or_default()
}

#[tauri::command]
pub fn set_start_pin(app: App, pinned: bool) -> Result<Vec<App>, String> {
    if app.id.trim().is_empty() {
        return Err("app has no id".into());
    }
    let current = load_start_pins(true)?;
    let next = apply_start_pin(&current, &app, pinned);
    save_start_pins(&next)?;
    Ok(next)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::io::Cursor;

    fn app(id: &str, name: &str) -> App {
        App {
            id: id.to_string(),
            name: name.to_string(),
            launch: format!("{id}.lnk"),
        }
    }

    fn catalog(id: &str, name: &str) -> CatalogApp {
        CatalogApp {
            app: app(id, name),
            category: "Other".to_string(),
        }
    }

    fn file(path: &str, ms: i64) -> FileEntry {
        FileEntry {
            name: path.rsplit('\\').next().unwrap_or(path).to_string(),
            path: path.to_string(),
            modified_ms: ms,
        }
    }

    /// One Favorites record: type byte, u32 LE length, body.
    fn record(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    /// An ITEMIDLIST with one item carrying `payload`, then the zero terminator.
    fn idlist(payload: &[u8]) -> Vec<u8> {
        let cb = (payload.len() + 2) as u16;
        let mut v = cb.to_le_bytes().to_vec();
        v.extend_from_slice(payload);
        v.extend_from_slice(&[0, 0]);
        v
    }

    #[test]
    fn split_reads_type_zero_records_in_order() {
        let first = idlist(b"ab");
        let second = idlist(b"cd");
        let blob = [record(0, &first), record(0, &second)].concat();
        assert_eq!(split_favorites(&blob), vec![first.as_slice(), second.as_slice()]);
    }

    #[test]
    fn split_skips_other_record_types_but_keeps_going() {
        let kept = idlist(b"ok");
        let blob = [record(7, &idlist(b"no")), record(0, &kept)].concat();
        assert_eq!(split_favorites(&blob), vec![kept.as_slice()]);
    }

    #[test]
    fn split_stops_on_truncated_header() {
        assert!(split_favorites(&[0, 1, 0]).is_empty());
        assert!(split_favorites(&[]).is_empty());
    }

    #[test]
    fn split_stops_on_overrunning_length_and_keeps_earlier_records() {
        let good = idlist(b"ok");
        let mut blob = record(0, &good);
        blob.extend_from_slice(&[0, 100, 0, 0, 0, 1, 2, 3]);
        assert_eq!(split_favorites(&blob), vec![good.as_slice()]);
    }

    #[test]
    fn split_stops_on_zero_length_record() {
        assert!(split_favorites(&[0, 0, 0, 0, 0, 9, 9]).is_empty());
    }

    #[test]
    fn idlist_validation_accepts_terminated_items() {
        assert!(is_well_formed_idlist(&idlist(b"xy")));
        assert!(is_well_formed_idlist(&[2, 0, 0, 0]));
    }

    #[test]
    fn idlist_validation_rejects_empty_list() {
        // Terminator at offset 0: no SHITEMID at all.
        assert!(!is_well_formed_idlist(&[0, 0]));
        assert!(!is_well_formed_idlist(&[0, 0, 9, 9]));
    }

    #[test]
    fn idlist_validation_rejects_missing_or_overrunning_terminator() {
        assert!(!is_well_formed_idlist(&[4, 0, 0x41, 0x42]));
        assert!(!is_well_formed_idlist(&[0xFF, 0x00, 0, 0]));
        assert!(!is_well_formed_idlist(&[1, 0, 0, 0]));
        assert!(!is_well_formed_idlist(&[]));
        assert!(!is_well_formed_idlist(&[0x02]));
    }

    #[test]
    fn categorize_matches_the_spec_examples() {
        assert_eq!(categorize("Google Chrome", ""), "Productivity");
        assert_eq!(categorize("Spotify", "SpotifyAB.SpotifyMusic!Spotify"), "Creativity");
        assert_eq!(categorize("Discord", ""), "Social");
        assert_eq!(categorize("Steam", ""), "Games");
        assert_eq!(categorize("Claude", "Claude_pzs8sxrjxfjjc!Claude"), "Developer tools");
        assert_eq!(categorize("Settings", ""), "Utilities");
        assert_eq!(categorize("Zither", "zither.app"), "Other");
    }

    #[test]
    fn categorize_first_rule_wins() {
        // "teams" is Social, but "visual studio" (Developer tools) comes later in the table.
        assert_eq!(categorize("Microsoft Teams Visual Studio", ""), "Social");
    }

    #[test]
    fn categorize_matches_whole_words_only() {
        // "git" must not match inside "Logitech".
        assert_eq!(categorize("Logitech G HUB", ""), "Other");
        assert_eq!(categorize("Git Bash", ""), "Developer tools");
        // "word" must not match inside "Password"; "code" not inside "Barcode".
        assert_eq!(categorize("Password Manager", ""), "Other");
        assert_eq!(categorize("Barcode Scanner", ""), "Other");
    }

    #[test]
    fn zoom_is_social_even_with_video_in_its_name() {
        assert_eq!(categorize("Zoom", "Zoom.Zoom"), "Social");
        assert_eq!(categorize("Zoom Video Communications", ""), "Social");
    }

    #[test]
    fn categorize_new_rules_on_this_machine() {
        assert_eq!(categorize("Figma", ""), "Creativity");
        assert_eq!(categorize("WebStorm", ""), "Developer tools");
        assert_eq!(categorize("IntelliJ IDEA", ""), "Developer tools");
        assert_eq!(categorize("PyCharm Community", ""), "Developer tools");
        assert_eq!(categorize("Rider", ""), "Developer tools");
        assert_eq!(categorize("CLion", ""), "Developer tools");
        assert_eq!(categorize("GoLand", ""), "Developer tools");
        assert_eq!(categorize("Roblox", ""), "Games");
        assert_eq!(categorize("EA", ""), "Games");
        assert_eq!(categorize("ea", ""), "Games");
        assert_eq!(categorize("EA app", ""), "Games");
        assert_eq!(categorize("Windows Software Development Kit", ""), "Developer tools");
    }

    #[test]
    fn skipped_entries_are_uninstallers_and_docs() {
        assert!(is_skipped_entry("Uninstall Foo"));
        assert!(is_skipped_entry("Foo README"));
        assert!(is_skipped_entry("Foo Release Notes"));
        assert!(is_skipped_entry("Foo Website"));
        assert!(is_skipped_entry("Windows Support Center"));
        assert!(is_skipped_entry("Tools for Desktop Apps"));
        assert!(is_skipped_entry("Sample Browser"));
        assert!(is_skipped_entry("Foo Documentation"));
        assert!(!is_skipped_entry("Google Chrome"));
    }

    #[test]
    fn catalog_skips_url_files_and_web_targets() {
        // .url internet shortcut.
        assert!(skip_catalog_entry("Project Site", Some(r"C:\Start\Project Site.url"), None));
        // .lnk whose target is a web address, in any case.
        assert!(skip_catalog_entry("Docs", Some(r"C:\Start\Docs.lnk"), Some("HTTPS://x.example")));
        assert!(skip_catalog_entry("Docs", None, Some("http://x.example")));
        // Ordinary apps stay.
        assert!(!skip_catalog_entry("Discord", Some(r"C:\Start\Discord.lnk"), Some(r"C:\Discord\Update.exe")));
        assert!(!skip_catalog_entry("Spotify", None, None));
        // Name rule still applies.
        assert!(skip_catalog_entry("Uninstall Discord", None, None));
    }

    #[test]
    fn cache_freshness_decision() {
        let now = Instant::now();
        let ttl = Duration::from_secs(30);
        assert!(!cache_is_fresh(None, now, ttl));
        assert!(cache_is_fresh(Some(now), now, ttl));
        assert!(cache_is_fresh(Some(now), now + Duration::from_secs(29), ttl));
        assert!(!cache_is_fresh(Some(now), now + Duration::from_secs(30), ttl));
        assert!(!cache_is_fresh(Some(now), now + Duration::from_secs(31), ttl));
    }

    #[test]
    fn finish_catalog_skips_sorts_and_dedupes_ids() {
        let items = vec![
            catalog("u", "Uninstall Foo"),
            catalog("z", "Zeta"),
            catalog("a", "alpha"),
            catalog("A", "Alpha dup"),
            catalog("r", "Readme"),
        ];
        let names: Vec<String> = finish_catalog(items)
            .into_iter()
            .map(|c| c.app.name)
            .collect();
        assert_eq!(names, vec!["alpha".to_string(), "Zeta".to_string()]);
    }

    #[test]
    fn search_ranks_prefix_first_case_insensitive_and_caps_at_six() {
        let names = ["Xbox Clock", "Clockwork", "clock", "Discord", "Alarm Clock"];
        let ranked = rank_by_name(names.iter(), "CLOCK", |n| *n, SEARCH_LIMIT);
        // Prefix matches first (input order: Clockwork, clock), then contains (Xbox Clock, Alarm Clock).
        assert_eq!(ranked, vec![&"Clockwork", &"clock", &"Xbox Clock", &"Alarm Clock"]);

        let many: Vec<String> = (0..10).map(|i| format!("app{i}")).collect();
        assert_eq!(rank_by_name(many, "APP", |s| s.as_str(), SEARCH_LIMIT).len(), 6);
    }

    #[test]
    fn search_empty_query_gives_nothing() {
        assert!(rank_by_name(vec![app("a", "Anything")], "   ", |a| a.name.as_str(), 6).is_empty());
    }

    #[test]
    fn recent_sorts_newest_first_and_dedupes_by_path() {
        let entries = vec![
            file(r"C:\a.txt", 100),
            file(r"C:\c.txt", 200),
            file(r"C:\A.TXT", 300),
        ];
        let out = finish_recent(entries, 10);
        let paths: Vec<&str> = out.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec![r"C:\A.TXT", r"C:\c.txt"]);
    }

    #[test]
    fn recent_limit_applies_after_sort() {
        let out = finish_recent(vec![file(r"C:\old.txt", 1), file(r"C:\new.txt", 2)], 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, r"C:\new.txt");
    }

    #[test]
    fn relative_depth_counts_components_below_root() {
        let root = Path::new(r"C:\Users\a\Desktop");
        assert_eq!(relative_depth(root, Path::new(r"C:\Users\a\Desktop\x.txt")), Some(1));
        assert_eq!(relative_depth(root, Path::new(r"C:\Users\a\Desktop\p\q\c.txt")), Some(3));
        assert_eq!(relative_depth(root, Path::new(r"C:\Users\a\Desktop")), Some(0));
        assert_eq!(relative_depth(root, Path::new(r"D:\other\x.txt")), None);
    }

    #[test]
    fn plain_path_targets_reject_uris_and_relative_paths() {
        assert!(is_plain_path_target(r"C:\Users\a\report.docx"));
        assert!(!is_plain_path_target("ms-settings:bluetooth"));
        assert!(!is_plain_path_target("https://example.com/x"));
        assert!(!is_plain_path_target("relative.txt"));
        assert!(!is_plain_path_target("  "));
    }

    #[test]
    fn start_pin_add_dedupes_and_remove_by_id() {
        let chrome = app("chrome", "Google Chrome");
        let added = apply_start_pin(&[], &chrome, true);
        assert_eq!(added, vec![chrome.clone()]);

        let again = apply_start_pin(&added, &app("CHROME", "Chrome copy"), true);
        assert_eq!(again, vec![chrome.clone()]);

        let spotify = app("spotify", "Spotify");
        let two = apply_start_pin(&added, &spotify, true);
        assert_eq!(two, vec![chrome.clone(), spotify.clone()]);

        let removed = apply_start_pin(&two, &app("Chrome", ""), false);
        assert_eq!(removed, vec![spotify]);
        assert_eq!(apply_start_pin(&removed, &app("nope", ""), false), removed);
    }

    #[test]
    fn strip_lnk_removes_only_the_extension() {
        assert_eq!(strip_lnk_suffix("Google Chrome.lnk"), "Google Chrome");
        assert_eq!(strip_lnk_suffix("Discord.LNK"), "Discord");
        assert_eq!(strip_lnk_suffix("Spotify"), "Spotify");
        assert_eq!(strip_lnk_suffix("ü"), "ü");
    }

    #[test]
    fn url_encoder_escapes_reserved_and_utf8() {
        assert_eq!(url_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(url_encode("Safe-_.~09"), "Safe-_.~09");
        assert_eq!(
            search_url("rust lang?"),
            "https://www.bing.com/search?q=rust%20lang%3F"
        );
    }

    #[test]
    fn skip_rules_for_search_walk() {
        assert!(is_skipped_dir_name(".git"));
        assert!(is_skipped_dir_name("node_modules"));
        assert!(!is_skipped_dir_name("Projects"));
        assert!(skip_attributes(FILE_ATTRIBUTE_HIDDEN));
        assert!(skip_attributes(FILE_ATTRIBUTE_SYSTEM));
        assert!(skip_attributes(FILE_ATTRIBUTE_REPARSE_POINT));
        assert!(!skip_attributes(0x10));
    }

    #[test]
    fn launch_target_shapes() {
        assert!(is_launch_target(r"shell:AppsFolder\SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"));
        assert!(!is_launch_target(r"shell:AppsFolder\"));
        assert!(!is_launch_target("relative.lnk"));
        assert!(is_launch_target(r"C:\ProgramData\Discord.lnk"));
    }

    #[test]
    fn sid_formats_binary_sid() {
        // S-1-5-32-33: revision 1, two sub-authorities, authority 5, 32 and 33.
        let bytes = [1, 2, 0, 0, 0, 0, 0, 5, 0x20, 0, 0, 0, 0x21, 0, 0, 0];
        assert_eq!(sid_to_string(&bytes).as_deref(), Some("S-1-5-32-33"));
        assert_eq!(sid_to_string(&bytes[..10]), None);
    }

    #[test]
    fn icon_alpha_zero_everywhere_is_opaque() {
        // BGRA: blue=10 green=20 red=30 alpha=0
        assert_eq!(bgra_premul_to_rgba(&[10, 20, 30, 0]), vec![30, 20, 10, 255]);
    }

    #[test]
    fn icon_unpremultiplies_partial_alpha() {
        // Premultiplied blue 64 at alpha 128 is straight blue 128.
        let out = bgra_premul_to_rgba(&[64, 0, 0, 128, 0, 0, 0, 0]);
        assert_eq!(out, vec![0, 0, 128, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn png_round_trips_pixels() -> Result<(), Box<dyn Error>> {
        let pixels: Vec<u8> = vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 9, 9, 9, 255];
        let bytes = encode_png(2, 2, &pixels).ok_or("encode failed")?;
        let mut reader = png::Decoder::new(Cursor::new(bytes)).read_info()?;
        let mut buf = vec![0; reader.output_buffer_size().ok_or("no buffer size")?];
        reader.next_frame(&mut buf)?;
        assert_eq!(buf, pixels);
        assert!(encode_png(2, 2, &pixels[..8]).is_none());
        Ok(())
    }

    #[test]
    fn pin_cache_same_bytes_is_cached_different_bytes_resolves_again() {
        let mut cache: Option<(Vec<u8>, Vec<App>)> = None;
        let mut runs = 0;
        let mut resolve = |tag: &str| {
            runs += 1;
            vec![app(tag, tag)]
        };

        let (first, ran) = cached_or_resolve(&mut cache, b"blob-a", || resolve("chrome"));
        assert!(ran);
        assert_eq!(first, vec![app("chrome", "chrome")]);

        let (again, ran) = cached_or_resolve(&mut cache, b"blob-a", || resolve("SHOULD-NOT-RUN"));
        assert!(!ran, "same bytes must hit the cache");
        assert_eq!(again, first);

        let (changed, ran) = cached_or_resolve(&mut cache, b"blob-b", || resolve("spotify"));
        assert!(ran, "different bytes must re-resolve");
        assert_eq!(changed, vec![app("spotify", "spotify")]);
        drop(resolve);
        assert_eq!(runs, 2);
    }

    #[test]
    fn image_mime_by_extension() {
        assert_eq!(image_mime(Path::new(r"C:\x\a.PNG")), Some("image/png"));
        assert_eq!(image_mime(Path::new(r"C:\x\a.jpeg")), Some("image/jpeg"));
        assert_eq!(image_mime(Path::new(r"C:\x\a.accountpicture-ms")), None);
    }

    #[test]
    fn start_pins_json_missing_key_or_bad_json() -> Result<(), Box<dyn Error>> {
        assert_eq!(parse_start_pins("{}")?, None);
        assert!(parse_start_pins("{not json").is_err());
        let text = r#"{"startPins":[{"id":"a","name":"A","launch":"x"},{"id":"A","name":"B","launch":"y"}]}"#;
        let pins = parse_start_pins(text)?.ok_or("missing pins")?;
        assert_eq!(pins.len(), 1);
        Ok(())
    }

    /// Needs the real shell and the user's taskbar. Run with `-- --ignored`.
    #[test]
    #[ignore = "needs the real shell and a Google Chrome taskbar pin; run with -- --ignored"]
    fn chrome_pin_icon_is_a_32px_png_with_pixels() -> Result<(), Box<dyn Error>> {
        let pins = pinned_apps();
        let chrome = pins
            .iter()
            .find(|a| a.name == "Google Chrome")
            .ok_or("no Google Chrome pin")?;
        let url = tauri::async_runtime::block_on(get_icon(chrome.launch.clone(), 32))
            .ok_or("get_icon returned None")?;
        let b64 = url
            .strip_prefix("data:image/png;base64,")
            .ok_or("not a PNG data URL")?;
        let bytes = STANDARD.decode(b64)?;
        let mut reader = png::Decoder::new(Cursor::new(bytes)).read_info()?;
        let (w, h) = {
            let info = reader.info();
            (info.width, info.height)
        };
        assert_eq!((w, h), (32, 32));
        let mut buf = vec![0; reader.output_buffer_size().ok_or("no buffer size")?];
        reader.next_frame(&mut buf)?;
        let visible = buf.chunks_exact(4).filter(|px| px[3] != 0).count();
        assert!(visible > 0, "icon has no non-transparent pixels");
        Ok(())
    }
}

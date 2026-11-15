# Lucent taskbar: build spec

The liquid-glass taskbar ("dock"), Start menu, search, quick settings and
notifications panels, as approved in design mockup v4. Nothing about the user's
apps is hardcoded: every app, icon, window, file and setting comes from Windows
at runtime. Only the look is fixed.

Shared contract: `src-tauri/src/model.rs` (JSON shapes, camelCase) and the stub
signatures in each module. Do not change a command's name or arguments, or a
model field, without updating this file and the UI.

## Windows (tauri.conf.json)

| label    | page          | notes |
|----------|---------------|-------|
| `island` | index.html    | the existing now-playing pill; untouched |
| `dock`   | dock.html     | transparent, non-focusable, always on top. Spans the primary monitor's full width, `DOCK_WINDOW_H` = 104 logical px tall, bottom-aligned |
| `flyout` | flyout.html   | transparent, focusable, always on top. Hosts one panel at a time |

## Geometry (logical px; multiply by the monitor's scale)

- Bar: 44 tall, 10 from the left, right and bottom edges of the monitor, radius 22.
- Dock window: monitor width x 104, bottom edge on the monitor's bottom edge.
  The top 60 px hold the hover label above an icon.
- App bar reservation (work area): bottom 64 px of the primary monitor.
- Flyout panels: bottom edge 12 px above the bar's top, so panel bottom =
  monitor bottom - 66. Flyout window = panel + `PAD` (24) on every side, so the
  spring overshoot is never clipped.
- Panel sizes: start 440 x 600, search 440 x 420, quick settings 300 x 300,
  notifications 320 x 480 (two stacked cards: notifications, calendar).
  Start and search are centred on the monitor; quick settings and
  notifications are right-aligned 10 px from the monitor's right edge.

## Look

Glass = CSS: each glass surface paints the desktop wallpaper (from
`get_backdrop`) at its screen position, blurred, so each region picks up the
wallpaper colours behind it. Native acrylic was tried and dropped: it ignores
window regions (draws a rectangle) and is solid when Windows transparency
effects are off. Then a dark smoky tint toned down from v3, then CSS
on top: a top shine gradient, 1 px inner highlights on all four edges, 0.5 px
outer hairline. Text #f2f2f7, secondary rgba(240,240,248,.6). Font Segoe UI
Variable. Spring: `cubic-bezier(.34,1.45,.64,1)` 550 ms; opacity 180 ms.

## Module `apps` (src/apps.rs)

- `pinned_apps()`: parse `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Taskband`
  value `Favorites` (REG_BINARY). Format, verified on this machine: repeated
  records `[u8 type=0][u32 LE len][ITEMIDLIST of len bytes]`; stop when the
  remaining bytes are fewer than 5 or len is 0 / overruns. Turn each ID list into
  an `IShellItem` (`SHCreateItemFromIDList`). For .lnk items (desktop apps), read
  the link's `System.AppUserModel.ID` (IPropertyStore on the item) and resolve the
  target path (IShellLinkW). For AppsFolder items (packaged apps), the parsing
  name is the AUMID. `App.id` = AUMID if present, else lowercase target exe path.
  `App.launch` = the item's file-system path (the .lnk) or `shell:AppsFolder\<AUMID>`.
  `App.name` = the item's normal display name without ".lnk".
  Expected on this machine, in order: File Explorer, Discord, Steam, Google
  Chrome, Spotify, Claude. Fallback when the value is missing or unparsable: the
  .lnk files in `%APPDATA%\Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar`
  sorted by name.
- `get_all_apps()`: enumerate `FOLDERID_AppsFolder` (`shell:AppsFolder`) once,
  cache it in a `OnceLock`/`Mutex`. Each item: name = display name, AUMID =
  parsing name, launch = `shell:AppsFolder\<AUMID>`, id = AUMID. Skip uninstallers
  and documentation entries (names containing "uninstall", "readme", "help",
  "documentation", "release notes", "website" case-insensitive). Sort by name.
- `categorize(name, aumid) -> &'static str`: pure, unit-tested keyword rules
  into `CATEGORIES` = ["Productivity", "Creativity", "Social", "Games",
  "Developer tools", "Utilities", "Other"]. Examples: chrome/edge/firefox/opera/
  office/word/excel/outlook/notion/pdf/acrobat/notepad -> Productivity;
  photo/paint/video/audio/music/spotify/obs/clipchamp/camera -> Creativity;
  discord/whatsapp/teams/slack/telegram/zoom/signal -> Social; steam/epic/riot/
  valorant/minecraft/xbox/game/battle.net/gog/ea app/ubisoft -> Games;
  code/visual studio/terminal/powershell/git/python/node/sdk/docker/wsl/claude/
  chatgpt/cursor/jetbrains/android studio -> Developer tools;
  settings/control panel/task manager/snipping/calculator/7-zip/everything/
  nvidia/amd/intel/defender/backup/cleanup/tool -> Utilities; else Other.
  Rules are data (a const table), first match wins.
- `get_icon(launch, size)`: `SHCreateItemFromParsingName(launch)` ->
  `IShellItemImageFactory::GetImage(size x size, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)`
  -> HBITMAP -> 32-bit BGRA via GetDIBits (top-down) -> RGBA (un-premultiply
  when needed) -> PNG (`png` crate) -> `data:image/png;base64,...`. Cache by
  (launch, size). COM: initialise apartment-threaded on the calling thread.
- `get_recent(limit)`: `.lnk` files in `%APPDATA%\Microsoft\Windows\Recent`,
  newest first by modified time, resolved to their targets; keep targets that
  exist and are files (not folders, not ms-* URIs); dedupe by target.
- `search(query)`: apps = cached All apps whose name contains the query
  (case-insensitive; prefix matches first; max 6). files = name matches among
  recent targets plus a walk of Desktop, Documents and Downloads (depth <= 3,
  stop after 5,000 entries scanned, skip hidden/system dirs and node_modules,
  .git), max 6, newest first. Empty query -> empty results.
- `launch(launch)`: `ShellExecuteExW` verb "open" on the parsing name
  (works for .lnk, .exe and `shell:AppsFolder\...`). `open_path(path)`: same,
  only for an existing path. `web_search(query)`: open
  `https://www.bing.com/search?q=<url-encoded query>` with the default browser.
- `get_user()`: `GetUserNameExW(NameDisplay)`, falling back to `%USERNAME%`.
  Avatar: newest image in `%APPDATA%\Microsoft\Windows\AccountPictures` (if
  any), else `C:\Users\Public\AccountPictures\<user SID>\*` (largest), as a
  data URL; None if not found.
- Start pins: Lucent's own list in `%APPDATA%\Lucent\config.json`
  (`{"startPins": [App, ...]}`); Windows' own Start pins are encrypted and not
  readable. First run seeds it with `pinned_apps()` and writes the file.
  `set_start_pin(app, pinned)` adds (append, no duplicates by id) or removes
  (by id) and returns the new list.
- `print_probe()`: one JSON object
  `{pinned, startPins, allAppsCount, categories: {cat: count}, recent: [first 6]}`.

## Module `tasks` (src/tasks.rs)

- A window is a taskbar window when: `IsWindowVisible`, not cloaked
  (`DWMWA_CLOAKED`), non-empty title, and either (no owner and no
  `WS_EX_TOOLWINDOW`) or `WS_EX_APPWINDOW`; not `WS_EX_NOACTIVATE` unless
  `WS_EX_APPWINDOW`; not owned by the Lucent process; class not in
  `Progman, WorkerW, Shell_TrayWnd, Shell_SecondaryTrayWnd`.
  Enumerate with `EnumWindows` (z-order, topmost first).
- Window identity: AUMID from `SHGetPropertyStoreForWindow` +
  `PKEY_AppUserModel_ID` if set; else for packaged processes
  `GetApplicationUserModelId` on the process; else the lowercase exe path
  (`QueryFullProcessImageNameW`). Explorer windows (class `CabinetWClass`) are
  `Microsoft.Windows.Explorer`.
- Matching a window to a pin (pure fn, unit-tested), first rule that holds:
  window AUMID == pin id; window exe path == pin target path (case-insensitive);
  pin .lnk arguments mention the window exe file name (Discord's
  `Update.exe --processStart Discord.exe`); window exe file stem equals the pin
  name or the pin target's file stem (case-insensitive).
- `DockItem` list = pins in pin order (with their windows), then unpinned running
  apps grouped by identity in order of first appearance. Unpinned name: AppsFolder
  display name for an AUMID, else the exe's FileDescription
  (`GetFileVersionInfoW`), else the window title. Unpinned launch: the exe path,
  or `shell:AppsFolder\<AUMID>`.
- Events: a dedicated thread with `SetWinEventHook` (out of context) for
  OBJECT_CREATE/SHOW/HIDE/DESTROY/NAMECHANGE/CLOAKED/UNCLOAKED, SYSTEM_FOREGROUND,
  SYSTEM_MINIMIZESTART/END, plus `RegisterShellHookWindow` on a hidden window for
  `HSHELL_FLASH` (wParam low word 0x8006 / HSHELL_REDRAW|HSHELL_HIGHBIT). Debounce
  100 ms with a one-shot timer like fullscreen.rs. On fire: rebuild, and emit
  `dock-changed` (payload `Vec<DockItem>`) to the `dock` window only when it
  changed. No polling. A window stops flashing when it becomes foreground.
- `activate_window(hwnd)`: if it is the foreground window -> minimise; else if
  minimised -> `ShowWindow(SW_RESTORE)`; then bring to front (simulate an Alt
  key tap with SendInput before `SetForegroundWindow` so the foreground lock
  allows it). `close_window(hwnd)`: `PostMessageW(WM_CLOSE)`.
- `print_probe()`: the current `Vec<DockItem>` as JSON.

## Module `system` (src/system.rs)

- Volume/mute: `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)`
  -> `IAudioEndpointVolume` (Get/SetMasterVolumeLevelScalar, Get/SetMute).
- Radios: WinRT `Windows.Devices.Radios.Radio::GetRadiosAsync`, kinds WiFi and
  Bluetooth; `wifi` / `bluetooth` = Some(state == On) if the radio exists.
  `set_radio("wifi"|"bluetooth", on)` -> `RequestAccessAsync` then `SetStateAsync`.
- Network name: `NetworkInformation::GetInternetConnectionProfile().ProfileName()`;
  `ethernet` = profile `IsWlanConnectionProfile() == false` and connected.
- Brightness: DDC/CI via dxva2 (`GetPhysicalMonitorsFromHMONITOR`,
  `GetMonitorBrightness`/`SetMonitorBrightness`) on the primary monitor; None
  when unsupported. Must not take longer than ~1 s; DDC is slow, run on the
  async thread pool.
- Battery: `GetSystemPowerStatus`; `battery` None when BatteryFlag has 128 (no
  battery) or 255 (unknown); `plugged_in` = ACLineStatus == 1.
- `open_settings(page)`: whitelist -> URI, opened with ShellExecute:
  nightlight `ms-settings:nightlight`, focus `ms-settings:focus`, network
  `ms-settings:network`, wifi `ms-settings:network-wifi`, bluetooth
  `ms-settings:bluetooth`, sound `ms-settings:sound`, display
  `ms-settings:display`, power `ms-settings:powersleep`, airplane
  `ms-settings:network-airplanemode`, settings `ms-settings:`, accounts
  `ms-settings:yourinfo`. Anything else -> Err.
- `send_shortcut(name)`: whitelist via SendInput: notifications = Win+N,
  quick_settings = Win+A, cast = Win+K. The shell module's Windows-key hook
  ignores injected input, so these reach Windows.
- `power(action)`: lock `LockWorkStation`; sleep `SetSuspendState(false,false,false)`;
  signout `ExitWindowsEx(EWX_LOGOFF)`; restart / shutdown: run
  `shutdown.exe /r /t 0` or `/s /t 0` (CREATE_NO_WINDOW). Else Err.
- `print_probe()`: `get_system()` as JSON.

## Module `glass` + `flyout` (src/glass.rs, src/flyout.rs)

- `glass::apply(window)`: acrylic through `SetWindowCompositionAttribute`
  (user32, loaded with GetProcAddress) with `ACCENT_ENABLE_ACRYLICBLURBEHIND`
  (4), `AccentFlags` 2, `GradientColor` in ABGR = tint rgba(24,24,32) at alpha
  0x66 (toned down). Fallback if that call fails: `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_TRANSIENTWINDOW)`
  after `DwmExtendFrameIntoClientArea(-1)`. Also set
  `DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_DONOTROUND` and
  `DWMWA_USE_IMMERSIVE_DARK_MODE = 1`.
- `set_regions(window, rects)`: union of `CreateRoundRectRgn` (logical ->
  physical with the window's scale factor; ellipse size = 2r) via `CombineRgn`,
  then `SetWindowRgn`. Region ownership as in island.rs. Empty list -> empty
  region (CreateRectRgn(0,0,0,0)).
- `flyout_open(panel, x, width, height, anchor_x)`: `panel` in
  {start, search, quick_settings, notifications}; `x` = panel's left edge and
  `anchor_x` = clicked icon's centre, both in logical px relative to the primary
  monitor's left edge; width/height = panel size. Positions the flyout window
  (panel + PAD on all sides; panel bottom at monitor bottom - 66), applies
  glass once, sets an empty region (the UI sets real regions per frame), shows
  it, focuses it (`set_focus`), then emits to the flyout window `flyout-show`
  `{panel, originX, originY}` where origin = the anchor point in the flyout
  window's logical coordinates (originY = centre of the bar: panel bottom + 12 + 22,
  relative to the window top). Emits `flyout-state` `{panel}` to the dock.
  If the flyout is already showing another panel, switch to the new one (same
  events). Keep the open panel in managed state.
- `flyout_request_close()`: emit `flyout-close` to the flyout window (the UI
  plays the reverse spring, then calls `flyout_hidden`). No-op when closed.
- `flyout_hidden()`: hide the window, clear state, emit `flyout-state`
  `{panel: null}` to the dock.
- `on_focus_lost(app)`: same as `flyout_request_close`, unless the dock was the
  thing clicked (the dock is non-activating, so focus does not move to it; this
  needs no special case).
- `force_hide(app)`: hide immediately and emit `flyout-state {panel: null}`.

## Module `shell` (src/shell.rs) + fullscreen.rs + autostart

- Takeover, in order: save the taskbar's current `ABM_GETSTATE` to
  `%APPDATA%\Lucent\taskbar-state.json` (only if no saved state exists, so a
  crash never overwrites the original), set `ABS_AUTOHIDE` via
  `ABM_SETSTATE` (frees the work area), hide `Shell_TrayWnd` and every
  `Shell_SecondaryTrayWnd` (`ShowWindow SW_HIDE`), register the dock HWND as an
  app bar (`ABM_NEW` with a callback message, `ABM_QUERYPOS`/`ABM_SETPOS` for the
  bottom 64 logical px of the primary monitor), place and show the dock
  (`SW_SHOWNOACTIVATE`), apply glass to the dock.
- Restore (idempotent): `ABM_REMOVE` the app bar, set the saved state back,
  show the tray windows, delete the saved-state file. Called from
  `RunEvent::Exit`, a panic hook (`std::panic::set_hook` that restores then
  calls the previous hook), and `--restore-taskbar`. `restore_cli` must work
  with no Tauri app running.
- Explorer restart: listen for `RegisterWindowMessageW("TaskbarCreated")` on a
  hidden window; re-hide the trays and re-set autohide; also call
  `flyout::force_hide`.
- Dock placement: primary monitor; window x = monitor x, width = monitor width,
  y = monitor bottom - 104*scale. Re-place on `WM_DISPLAYCHANGE`,
  `WM_SETTINGCHANGE` (work area) and scale changes (`place_dock`).
- Fullscreen: in fullscreen.rs `apply`, also compute whether the primary
  monitor has the fullscreen foreground window and call
  `shell::set_dock_suppressed(app, busy)`. Suppressed = hide the dock with raw
  `ShowWindow(SW_HIDE)` and `flyout::force_hide`; unsuppressed = place and
  `SW_SHOWNOACTIVATE`. Our own windows being foreground (Fg::Ours) never changes it.
- Windows key: `WH_KEYBOARD_LL` hook on a dedicated thread with a message loop.
  Ignore injected events (`LLKHF_INJECTED`). Track LWin/RWin down; if any other
  key goes down while Win is held, it is a combo (pass through untouched). On a
  lone Win key-up: inject a masking key tap (vk 0xE8, down+up) *before* letting
  the Win up through, so Windows' Start does not open, then emit `start-key` to
  the dock window (on the main thread). Never swallow a combo.
- `show_native_tray`: show `Shell_TrayWnd` (`SW_SHOW`) and bring it to the
  foreground so the user can reach tray icons; a foreground WinEvent re-hides it
  as soon as the foreground moves to something that is not a shell tray window
  (classes `Shell_TrayWnd`, `NotifyIconOverflowWindow`,
  `TopLevelWindowForOverflowXamlIsland`, `Xaml_WindowedPopupClass`) and not Lucent.
- Lucent's own tray host window (module `tray`) also has class `Shell_TrayWnd`;
  `tray_windows` skips windows owned by Lucent so it is never shown or hidden.
- `show_native_tray` is now only the fallback when the tray host failed to start.

## Module `tray` (src-tauri/src/tray.rs)

Notification-area host on its own thread and message loop:

- Creates a hidden `WS_EX_TOPMOST | WS_EX_TOOLWINDOW` popup of class
  `Shell_TrayWnd`, raised above Explorer's, so `Shell_NotifyIcon` (which sends
  `WM_COPYDATA` to the first `Shell_TrayWnd` in z-order) reaches Lucent. Allows
  `WM_COPYDATA` and `TaskbarCreated` from lower integrity.
- `WM_COPYDATA` with `dwData == 1`: parse the signed payload (signature
  `0x34753423`, NIM_* command, NOTIFYICONDATAW with 32-bit handles), apply
  add/modify/delete/setversion to the icon store (identity: GUID with
  `NIF_GUID`, else hWnd + uID), render the HICON to a PNG data URL while the
  caller still waits, emit `tray-changed`. Every `WM_COPYDATA` (also appbar
  `dwData 0` and icon-rect `dwData 3`), `WM_COMMAND` and `WM_USER..0x7FFF`
  message is forwarded to Explorer's tray, so Explorer keeps a full copy.
- At start, posts `TaskbarCreated` so running apps re-register. On a
  `TaskbarCreated` from a restarted Explorer, re-raises and re-broadcasts (our
  own broadcast arrives while we are first, so it does not loop). WinEvents
  (show, foreground of a `Shell_TrayWnd`) re-raise the host; no polling.
- Commands: `get_tray_icons` (visible icons, dead owners dropped; errors when
  the host is not running) and `tray_click(id, button)` with `left`, `right`,
  `double`: `AllowSetForegroundWindow(owner pid)`, then the Explorer sequence
  (down, up, plus `NIN_SELECT` / `WM_CONTEXTMENU` for version 3+). Version 4:
  `wParam` = cursor x/y, `lParam` = event | uID << 16; older: `wParam` = uID,
  `lParam` = event.
- On exit, `tray::stop` closes the host before the taskbar is restored.

Tray panel: the chevron opens the `tray` flyout panel, a glass grid of 36 px
cells (4 columns, 10 px padding) sized by the dock from the icon count and
resized live on `tray-changed`.

- Autostart: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value
  `Lucent` = `"<current exe path>"`. `autostart_enabled()` = the value exists
  and points at the current exe.

## UI (ui/dock.*, ui/flyout.*, ui/glass.css)

Plain HTML/CSS/JS, no build step, `window.__TAURI__` global, CSP `script-src
'self'` (no inline scripts or handlers). Icons are the apps' real icons from
`get_icon`; UI glyphs (start, search, wifi, volume, bell, chevron, power) are
inline SVG.

Dock: full-width bar. Centre group: start, search, separator, then `DockItem`s.
Right group: chevron (tray panel), quick-settings button (wifi or
ethernet glyph + volume glyph), clock button (time + short date, refreshed at
each minute boundary) with bell. Running dot under icons with windows; wider
white bar + lit tile for `active`; red dot for `flashing`. Hover: icon lifts
5 px and scales 1.12, a glass label with the app name appears above it (dock
updates its regions to include the label). Click: not running -> `launch`; one
window -> `activate_window`; several -> activate the topmost, cycling on
repeated clicks. Middle-click -> `launch` (new instance). Right-click on a
running item -> close its windows is NOT offered in v1. Regions: the bar (and
the hover label when shown) via `set_regions`, refreshed on resize/layout.
Listens: `dock-changed`, `flyout-state` (lights start/search/quick
settings/clock buttons), `start-key` (toggle Start).
Toggling: clicking the button of the open panel -> `flyout_request_close`;
another button -> `flyout_open` with that panel.

Flyout: one page, four panels (only the open one rendered visible). On
`flyout-show`: set `transform-origin` to origin, play the spring from
scale(.12) translateY(24px) opacity 0 to identity, and every animation frame
send `set_regions` with the panel's current bounding rect(s)
(getBoundingClientRect, radius scaled by the same factor) until the
transition ends. On `flyout-close` or Escape: reverse, then `flyout_hidden`.
- Start: search field (typing switches to the search panel content inside the
  Start panel, Win11-style); Pinned grid (6 columns, `get_start_pins`, icons
  via `get_icon(launch, 32)`); Recent (2 columns, `get_recent(4)`, file-type
  glyph, relative time "Yesterday, 9:32 PM"); All apps by category
  (`get_all_apps`, 4 category tiles showing the first 4 icons; clicking a
  tile expands that category's list in place); footer with `get_user` and a
  power button opening a small glass menu (Lock, Sign out, Sleep, Restart,
  Shut down -> `power`). Right-click on an app -> Pin/Unpin to Start
  (`set_start_pin`).
- Search: focused input, debounce 120 ms -> `search`; best-match card + Apps,
  Files, Web ("Search the web for ..." -> `web_search`). Enter launches the
  best match. Arrow keys move the selection.
- Quick settings: toggles Wi-Fi, Bluetooth (`set_radio`, hidden if None),
  Night light, Focus, Airplane, Cast (open settings / `send_shortcut cast`;
  shown as shortcut tiles with a small arrow, not fake toggles); brightness
  slider (hidden if None) and volume slider + mute; footer with power status
  and a settings gear (`open_settings settings`). Refresh with `get_system`
  each time the panel opens.
- Notifications: top card "Notifications" with a button "Open notification
  centre" (`send_shortcut notifications`; Windows only lets packaged apps read
  notifications); calendar card with today's long date and the month grid,
  today highlighted, prev/next month arrows.

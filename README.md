# Lucent

Liquid-glass shell customizations for Windows 11 — starting with a Dynamic-Island-style now-playing pill (dock and widgets planned).

## Features

- Dynamic-Island-style "now playing" pill at the top centre of the screen, showing the current Windows media session (artwork, equaliser while playing).
- Hover to expand: artwork, title, artist, a playback timeline (elapsed, remaining, progress bar), and previous / play-pause / next controls. The timeline advances only while the card is open and a track is playing.
- Briefly expands when the track changes.
- Event-driven and light on resources: the media monitor blocks until Windows reports a change, so there is no polling and idle CPU stays near zero.
- `lucent.exe --print-media` prints the current media state as one line of JSON and exits.
- Steps aside for fullscreen apps (games, video players): when the app is fullscreen on the primary monitor, the pill moves to the first other monitor without one, and hides when no monitor is free. It returns to the primary monitor when the app leaves fullscreen. Event-driven (Windows event hooks), no polling.
- System tray icon with "Quit Lucent" (there is no taskbar entry).

## Build

Prerequisites:

- Rust (stable, MSVC toolchain)
- Visual Studio Build Tools with the C++ workload
- WebView2 runtime (preinstalled on current Windows 11)

Run:

```
cargo run --manifest-path src-tauri/Cargo.toml
```

Optional: `py scripts/make_icon.py` regenerates the committed icon (only needed if you change it).

Tests:

```
cargo test --manifest-path src-tauri/Cargo.toml
```

## Install

Builds the release exe and installs it for the current user (`%LOCALAPPDATA%\Programs\Lucent`), adds a Start Menu shortcut, enables start with Windows, and starts Lucent:

```
powershell -ExecutionPolicy Bypass -File scripts\install.ps1
```

Remove it again with `-Uninstall`, which also puts the taskbar back:

```
powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -Uninstall
```

## Recovery

Lucent hides the Windows taskbar while it runs and puts it back on exit. If Lucent was killed or crashed and the taskbar is still hidden, run:

```
lucent.exe --restore-taskbar
```

It works with no other Lucent process running. It restores the taskbar's auto-hide setting from `%APPDATA%\Lucent\taskbar-state.json` and deletes that file. With no state file, it only shows the taskbar and leaves the auto-hide setting alone. Lucent refuses to take over while that file is unreadable, so delete it by hand only after the taskbar is visible again.

## Roadmap

- Liquid-glass dock
- Top bar
- Desktop widgets
- File sharing based on LocalSend
- AirDrop / AWDL research

## License

MIT. See [LICENSE](LICENSE).

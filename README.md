# Lucent

Liquid-glass shell customizations for Windows 11 — starting with a Dynamic-Island-style now-playing pill (dock and widgets planned).

## Features

- Dynamic-Island-style "now playing" pill at the top centre of the screen, showing the current Windows media session (artwork, equaliser while playing).
- Hover to expand: artwork, title, artist, a playback timeline (elapsed, remaining, progress bar), and previous / play-pause / next controls. The timeline advances only while the card is open and a track is playing.
- Briefly expands when the track changes.
- Event-driven and light on resources: the media monitor blocks until Windows reports a change, so there is no polling and idle CPU stays near zero.
- `lucent.exe --print-media` prints the current media state as one line of JSON and exits.
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

## Roadmap

- Liquid-glass dock
- Top bar
- Desktop widgets
- File sharing based on LocalSend
- AirDrop / AWDL research
- Pause while fullscreen games are running

## License

MIT. See [LICENSE](LICENSE).

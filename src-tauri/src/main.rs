#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod fullscreen;
mod island;
mod media;

use base64::Engine as _;
use serde_json::json;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{App, Manager};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};

/// `lucent --print-media`: print the current media state as one JSON line.
fn print_media_probe() {
    // Release builds have no console; attach to the parent shell if there is one.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    let state = media::probe();
    let mut value = serde_json::to_value(&state).unwrap_or(serde_json::Value::Null);
    if let Some(url) = &state.artwork {
        // Report the decoded image size, not the length of the data URL.
        let decoded = url
            .split_once("base64,")
            .and_then(|(_, b64)| base64::engine::general_purpose::STANDARD.decode(b64).ok())
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        value["artwork"] = json!(format!("<{decoded} bytes>"));
    }
    println!("{value}");
}

fn build_tray(app: &App) -> tauri::Result<()> {
    let quit = MenuItem::with_id(app, "quit", "Quit Lucent", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&quit])?;
    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("Lucent")
        .on_menu_event(|app, event| {
            if event.id().0 == "quit" {
                app.exit(0);
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

fn main() {
    if std::env::args().any(|arg| arg == "--print-media") {
        print_media_probe();
        return;
    }

    tauri::Builder::default()
        .setup(|app| {
            let media = media::start(app.handle().clone());
            app.manage(media);
            app.manage(island::IslandPlacement::default());
            island::show_initial(app.handle())?;
            fullscreen::start(app.handle().clone());
            build_tray(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            island::set_expanded,
            media::get_media_state,
            media::media_toggle,
            media::media_next,
            media::media_prev,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Lucent");
}

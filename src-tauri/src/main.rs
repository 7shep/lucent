#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apps;
mod flyout;
mod fullscreen;
mod glass;
mod island;
mod media;
mod model;
mod shell;
mod system;
mod tasks;

use base64::Engine as _;
use serde_json::json;
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
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
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start with Windows",
        true,
        shell::autostart_enabled(),
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit Lucent", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&autostart, &quit])?;
    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("Lucent")
        .on_menu_event(move |app, event| match event.id().0.as_str() {
            "quit" => app.exit(0),
            "autostart" => {
                let wanted = !shell::autostart_enabled();
                if let Err(err) = shell::set_autostart(wanted) {
                    eprintln!("lucent: autostart change failed: {err}");
                }
                let _ = autostart.set_checked(shell::autostart_enabled());
            }
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// Attaches to the parent console so CLI probes can print from a release build.
fn attach_console() {
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// True when another Lucent is already running in this session. The mutex
/// handle is leaked on purpose so it lives as long as the process.
fn already_running() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    match unsafe { CreateMutexW(None, false, w!("Local\\LucentShell")) } {
        Ok(_handle) => (unsafe { GetLastError() }) == ERROR_ALREADY_EXISTS,
        Err(_) => false,
    }
}

/// True when a Lucent instance holds the single-instance mutex, without
/// creating it.
fn instance_running() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, w!("Local\\LucentShell")) } {
        Ok(handle) => {
            let _ = unsafe { CloseHandle(handle) };
            true
        }
        Err(_) => false,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    if has("--print-media") {
        print_media_probe();
        return;
    }
    if has("--print-apps") {
        attach_console();
        apps::print_probe();
        return;
    }
    if has("--print-dock") {
        attach_console();
        tasks::print_probe();
        return;
    }
    if has("--print-system") {
        attach_console();
        system::print_probe();
        return;
    }
    if has("--print-backdrop") {
        attach_console();
        glass::print_probe();
        return;
    }
    if has("--restore-taskbar") {
        attach_console();
        if instance_running() {
            // The running instance would hide the taskbar again at once.
            eprintln!("lucent: Lucent is running; quit it from its tray icon to restore the taskbar");
            std::process::exit(1);
        }
        shell::restore_cli();
        return;
    }
    if already_running() {
        return;
    }

    let app = tauri::Builder::default()
        // Managed before any window exists: window events can arrive while the
        // configured windows are still being created, before setup runs.
        .manage(island::IslandPlacement::default())
        .setup(|app| {
            let media = media::start(app.handle().clone());
            app.manage(media);
            island::show_initial(app.handle())?;
            tasks::start(app.handle().clone());
            shell::takeover(app.handle())?;
            fullscreen::start(app.handle().clone());
            build_tray(app)?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Lucent's windows live for the whole session: Alt+F4 must not
            // destroy them (the flyout is focusable, so it can receive it).
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                if window.label() == flyout::LABEL {
                    flyout::force_hide(window.app_handle());
                }
            }
            tauri::WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if window.label() == island::LABEL {
                    island::on_scale_changed(window.app_handle(), *scale_factor);
                } else if window.label() == shell::DOCK_LABEL {
                    shell::place_dock(window.app_handle());
                }
            }
            tauri::WindowEvent::Focused(false) if window.label() == flyout::LABEL => {
                flyout::on_focus_lost(window.app_handle());
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            island::set_expanded,
            island::set_hit_region,
            media::get_media_state,
            media::media_toggle,
            media::media_next,
            media::media_prev,
            apps::get_pinned,
            apps::get_all_apps,
            apps::get_icon,
            apps::get_recent,
            apps::search,
            apps::launch,
            apps::open_path,
            apps::web_search,
            apps::get_user,
            apps::get_start_pins,
            apps::set_start_pin,
            tasks::get_dock,
            tasks::activate_window,
            tasks::close_window,
            system::get_system,
            system::set_volume,
            system::set_muted,
            system::set_radio,
            system::set_brightness,
            system::open_settings,
            system::send_shortcut,
            system::power,
            glass::set_regions,
            glass::get_backdrop,
            flyout::flyout_open,
            flyout::flyout_request_close,
            flyout::flyout_hidden,
            shell::show_native_tray,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Lucent");

    app.run(|_app, event| {
        // Every way out of the event loop puts the Windows taskbar back.
        if let tauri::RunEvent::Exit = event {
            shell::restore_quiet();
        }
    });
}

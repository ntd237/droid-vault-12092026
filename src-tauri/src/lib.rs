// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
pub mod adb;
pub mod apk;
pub mod arsc_resolve;
pub mod cache;
pub mod commands;
pub mod config;
pub mod packages;
pub mod vector;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            #[cfg(desktop)]
            {
                app.handle()
                    .plugin(tauri_plugin_updater::Builder::new().build())?;
                app.handle().plugin(tauri_plugin_process::init())?;
            }
            Ok(())
        })
        .manage(commands::AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::list_devices,
            commands::get_apps,
            commands::get_apps_signature,
            commands::uninstall_apps,
            commands::restore_apps,
            commands::get_config,
            commands::set_theme
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

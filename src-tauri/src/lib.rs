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
        .manage(commands::AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::list_devices,
            commands::get_apps,
            commands::uninstall_apps,
            commands::restore_apps,
            commands::get_config
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

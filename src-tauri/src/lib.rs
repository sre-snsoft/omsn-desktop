pub mod commands;
pub mod config;
pub mod error;
pub mod lark;
pub mod repo;
pub mod sync;
pub mod task;

use commands::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Err(err) = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::sign_in,
            commands::current_viewer,
            commands::list_my_tasks,
            commands::update_task,
            commands::create_task,
            commands::delete_task,
        ])
        .run(tauri::generate_context!())
    {
        eprintln!("OMSN Desktop failed to start: {err}");
        std::process::exit(1);
    }
}

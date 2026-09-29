pub mod error;
pub mod task;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    if let Err(err) = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .run(tauri::generate_context!())
    {
        eprintln!("OMSN Desktop failed to start: {err}");
        std::process::exit(1);
    }
}

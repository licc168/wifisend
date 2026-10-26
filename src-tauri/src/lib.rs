mod hub;
mod server;

use hub::{FileItem, SessionInfo, Shared};
use std::sync::Arc;
use tauri::Manager;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
async fn session(hub: tauri::State<'_, Shared>) -> Result<SessionInfo, String> {
    hub.wait_ready().await?;
    hub.session()
}

#[tauri::command]
fn list_files(hub: tauri::State<'_, Shared>) -> Vec<FileItem> {
    hub.snapshot()
}

#[tauri::command]
fn delete_file(hub: tauri::State<'_, Shared>, name: String) -> Result<(), String> {
    hub.delete(&name)
}

#[tauri::command]
fn clear_files(hub: tauri::State<'_, Shared>) -> Result<usize, String> {
    hub.clear()
}

#[tauri::command]
async fn import_paths(
    hub: tauri::State<'_, Shared>,
    paths: Vec<String>,
) -> Result<Vec<FileItem>, String> {
    let shared = Arc::clone(&hub);
    tokio::task::spawn_blocking(move || shared.import_paths(&paths, "这台电脑", "desktop"))
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
async fn pick_and_import(
    directory: bool,
    app: tauri::AppHandle,
    hub: tauri::State<'_, Shared>,
) -> Result<Vec<FileItem>, String> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title(if directory {
        "选择要发送的文件夹"
    } else {
        "选择要发送的文件"
    });
    if let Some(window) = app.get_webview_window("main") {
        dialog = attach_parent(dialog, &window);
    }
    let paths = if directory {
        dialog
            .pick_folder()
            .await
            .map(|folder| vec![folder.path().display().to_string()])
            .unwrap_or_default()
    } else {
        dialog
            .pick_files()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|file| file.path().display().to_string())
            .collect()
    };
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let shared = Arc::clone(&hub);
    tokio::task::spawn_blocking(move || shared.import_paths(&paths, "这台电脑", "desktop"))
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
fn save_text(hub: tauri::State<'_, Shared>, text: String) -> Result<FileItem, String> {
    hub.save_text(&text, "这台电脑", "desktop")
}

#[tauri::command]
fn read_text_file(hub: tauri::State<'_, Shared>, name: String) -> Result<String, String> {
    hub.read_text(&name)
}

#[tauri::command]
fn open_share_dir(app: tauri::AppHandle, hub: tauri::State<'_, Shared>) -> Result<(), String> {
    open_path(&app, &hub.share_dir)
}

#[tauri::command]
fn open_file(app: tauri::AppHandle, hub: tauri::State<'_, Shared>, name: String) -> Result<(), String> {
    let path = hub::resolve_file(&hub.share_dir, &name).ok_or_else(|| "没有这个文件".to_string())?;
    open_path(&app, &path)
}

#[tauri::command]
fn reveal_file(app: tauri::AppHandle, hub: tauri::State<'_, Shared>, name: String) -> Result<(), String> {
    let path = hub::resolve_file(&hub.share_dir, &name).ok_or_else(|| "没有这个文件".to_string())?;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|err| err.to_string())
}

fn open_path(app: &tauri::AppHandle, path: &std::path::Path) -> Result<(), String> {
    app.opener()
        .open_path(path.display().to_string(), None::<&str>)
        .map_err(|err| err.to_string())
}

fn attach_parent(
    dialog: rfd::AsyncFileDialog,
    window: &tauri::WebviewWindow,
) -> rfd::AsyncFileDialog {
    #[cfg(target_os = "windows")]
    {
        return dialog.set_parent(window);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = window;
        dialog
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let hub = Arc::new(hub::Hub::open());
            hub.attach(app.handle().clone());
            app.manage(hub.clone());
            tauri::async_runtime::spawn(async move {
                if let Err(err) = server::serve(hub).await {
                    eprintln!("快传服务停止了: {err}");
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            session,
            list_files,
            delete_file,
            clear_files,
            import_paths,
            pick_and_import,
            save_text,
            read_text_file,
            open_share_dir,
            open_file,
            reveal_file
        ])
        .run(tauri::generate_context!())
        .expect("快传窗口启动失败");
}

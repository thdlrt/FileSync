mod models;
mod state;
mod sync_engine;

use crate::models::{AppLogEntry, AppSettings, AppStateSnapshot, CleanupPreview, RuleDraft, SyncRule};
use crate::state::SharedState;
use std::path::PathBuf;
use std::process::Command;
use tauri::menu::MenuBuilder;
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, State, WindowEvent};

#[tauri::command]
async fn get_app_state(state: State<'_, SharedState>) -> Result<AppStateSnapshot, String> {
    state.snapshot().await
}

#[tauri::command]
async fn list_rules(state: State<'_, SharedState>) -> Result<Vec<SyncRule>, String> {
    state.list_rules().await
}

#[tauri::command]
async fn create_rule(
    rule: RuleDraft,
    state: State<'_, SharedState>,
) -> Result<AppStateSnapshot, String> {
    state.create_rule(rule).await
}

#[tauri::command]
async fn update_rule(
    rule_id: String,
    rule: RuleDraft,
    state: State<'_, SharedState>,
) -> Result<AppStateSnapshot, String> {
    state.update_rule(rule_id, rule).await
}

#[tauri::command]
async fn delete_rule(
    rule_id: String,
    state: State<'_, SharedState>,
) -> Result<AppStateSnapshot, String> {
    state.delete_rule(rule_id).await
}

#[tauri::command]
async fn run_rule_sync(
    rule_id: String,
    state: State<'_, SharedState>,
) -> Result<AppStateSnapshot, String> {
    state.run_rule_sync_command(rule_id).await
}

#[tauri::command]
async fn run_all_sync(state: State<'_, SharedState>) -> Result<AppStateSnapshot, String> {
    state.run_all_sync_command().await
}

#[tauri::command]
async fn preview_rule_cleanup(
    rule_id: String,
    state: State<'_, SharedState>,
) -> Result<CleanupPreview, String> {
    state.preview_cleanup(rule_id).await
}

#[tauri::command]
async fn execute_rule_cleanup(
    rule_id: String,
    state: State<'_, SharedState>,
) -> Result<AppStateSnapshot, String> {
    state.execute_cleanup(rule_id).await
}

#[tauri::command]
async fn toggle_rule_enabled(
    rule_id: String,
    enabled: bool,
    state: State<'_, SharedState>,
) -> Result<AppStateSnapshot, String> {
    state.toggle_rule_enabled(rule_id, enabled).await
}

#[tauri::command]
async fn get_settings(state: State<'_, SharedState>) -> Result<AppSettings, String> {
    state.get_settings().await
}

#[tauri::command]
async fn get_logs(state: State<'_, SharedState>) -> Result<Vec<AppLogEntry>, String> {
    state.get_logs().await
}

#[tauri::command]
async fn clear_history(state: State<'_, SharedState>) -> Result<AppStateSnapshot, String> {
    state.clear_history().await
}

#[tauri::command]
async fn clear_logs(state: State<'_, SharedState>) -> Result<Vec<AppLogEntry>, String> {
    state.clear_logs().await
}

#[tauri::command]
fn get_log_path(state: State<'_, SharedState>) -> Result<String, String> {
    state.get_log_path()
}

#[tauri::command]
fn open_with_default_app(path: String) -> Result<(), String> {
    let target = PathBuf::from(&path);
    if !target.exists() {
        return Err(format!("路径不存在：{}", target.display()));
    }

    Command::new("cmd")
        .args(["/C", "start", "", &path])
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    let target = PathBuf::from(&path);
    if target.is_file() {
        Command::new("explorer")
            .arg(format!("/select,{}", target.display()))
            .spawn()
            .map_err(|error| error.to_string())?;
        return Ok(());
    }

    let directory = if target.exists() {
        target
    } else {
        target
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| "找不到可打开的父目录。".to_string())?
    };

    if !directory.exists() {
        return Err(format!("路径不存在：{}", directory.display()));
    }

    Command::new("explorer")
        .arg(directory)
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
async fn save_settings(
    settings: AppSettings,
    state: State<'_, SharedState>,
) -> Result<AppStateSnapshot, String> {
    state.save_settings(settings).await
}

#[tauri::command]
async fn set_auto_sync_paused(
    paused: bool,
    state: State<'_, SharedState>,
) -> Result<AppStateSnapshot, String> {
    state.set_auto_sync_paused(paused).await
}

fn reveal_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let menu = MenuBuilder::new(app)
        .text("show", "显示主窗口")
        .text("sync_now", "立即全部同步")
        .text("toggle_pause", "暂停 / 恢复自动同步")
        .text("settings", "打开设置")
        .separator()
        .text("quit", "退出")
        .build()?;

    let mut tray = TrayIconBuilder::with_id("main-tray")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => reveal_main_window(app),
            "sync_now" => {
                if let Some(state) = app.try_state::<SharedState>() {
                    let shared = state.inner().clone();
                    tauri::async_runtime::spawn(async move {
                        let _ = shared.run_all_sync_command().await;
                    });
                }
            }
            "toggle_pause" => {
                if let Some(state) = app.try_state::<SharedState>() {
                    let shared = state.inner().clone();
                    tauri::async_runtime::spawn(async move {
                        if let Ok(snapshot) = shared.snapshot().await {
                            let _ = shared
                                .set_auto_sync_paused(!snapshot.automatic_sync_paused)
                                .await;
                        }
                    });
                }
            }
            "settings" => reveal_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        });

    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }

    tray.build(app)?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            reveal_main_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let shared = SharedState::new(app.handle().clone())?;
            tauri::async_runtime::block_on(shared.initialize())?;
            app.manage(shared);
            build_tray(app)?;

            if let Some(window) = app.get_webview_window("main") {
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        if let Some(state) = app_handle.try_state::<SharedState>() {
                            let close_to_tray = tauri::async_runtime::block_on(async {
                                state
                                    .get_settings()
                                    .await
                                    .map(|settings| settings.close_to_tray)
                            })
                            .unwrap_or(true);
                            if close_to_tray {
                                api.prevent_close();
                                if let Some(main_window) = app_handle.get_webview_window("main") {
                                    let _ = main_window.hide();
                                }
                            }
                        }
                    }
                });

                let start_hidden = tauri::async_runtime::block_on(async {
                    app.state::<SharedState>()
                        .get_settings()
                        .await
                        .map(|settings| settings.start_minimized_to_tray)
                })
                .unwrap_or(false);

                if start_hidden {
                    let _ = window.hide();
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_state,
            list_rules,
            create_rule,
            update_rule,
            delete_rule,
            run_rule_sync,
            run_all_sync,
            preview_rule_cleanup,
            execute_rule_cleanup,
            toggle_rule_enabled,
            get_settings,
            get_logs,
            clear_history,
            clear_logs,
            get_log_path,
            open_with_default_app,
            reveal_path,
            save_settings,
            set_auto_sync_paused
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

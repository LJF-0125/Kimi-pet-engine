mod kimi;

use tauri::Manager;

/// 双击桌宠：返回本地 kimi web 的完整访问地址（含 token）。
#[tauri::command]
async fn web_url() -> Option<String> {
    kimi::web_url().await
}

/// 前端启动后主动拉取当前状态（tauri 事件不重放，listen 注册前的 emit 会丢）。
#[tauri::command]
fn get_state() -> &'static str {
    kimi::current_state()
}

/// 右键桌宠：打开设置窗口。
#[tauri::command]
fn open_settings(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = tauri::WebviewWindowBuilder::new(
        &app,
        "settings",
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title("桌宠设置")
    .inner_size(520.0, 460.0)
    .resizable(true)
    .build();
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![web_url, get_state, open_settings])
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(kimi::run(handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running kimi-pet");
}

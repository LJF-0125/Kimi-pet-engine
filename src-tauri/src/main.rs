// release 构建不带控制台窗口；debug 保留控制台方便看日志
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod edge_hide;
mod kimi;
mod launch_hook;

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

/// 打开（或聚焦）设置窗口。
fn show_settings(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title("桌宠设置")
    .inner_size(520.0, 650.0)
    .resizable(true)
    .build();
}

/// 右键桌宠：打开设置窗口。
#[tauri::command]
async fn open_settings(app: tauri::AppHandle) {
    show_settings(&app);
}

/// 调整桌宠大小：按 tauri.conf.json 的基准尺寸 220x220 等比缩放主窗口，
/// 让可拖动/遮挡区域始终和 GIF 视觉大小一致。
#[tauri::command]
fn set_scale(app: tauri::AppHandle, scale: f64) {
    if let Some(w) = app.get_webview_window("main") {
        let scale = scale.clamp(0.5, 2.5);
        let _ = w.set_size(tauri::LogicalSize::new(220.0 * scale, 220.0 * scale));
    }
}

/// 托盘回调在事件循环线程上，直接建窗口可能死锁（同同步命令的坑），丢到异步运行时。
fn spawn_show_settings(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        show_settings(&handle);
    });
}

fn main() {
    tauri::Builder::default()
        // 单实例：hook / 手动重复启动时只保留先运行的实例
        .plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}))
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            web_url,
            get_state,
            open_settings,
            set_scale,
            launch_hook::get_launch_hook,
            launch_hook::set_launch_hook,
            kimi::set_auto_quit,
            edge_hide::set_hide_mode,
            edge_hide::set_head_rect,
            edge_hide::reveal_if_hidden,
            edge_hide::try_rehide
        ])
        .setup(|app| {
            // 系统托盘：左键单击 → 设置；右键菜单 → 桌宠设置 / 退出
            let settings = tauri::menu::MenuItem::with_id(app, "settings", "桌宠设置", true, None::<&str>)?;
            let quit = tauri::menu::MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&settings, &quit])?;
            let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))?;
            tauri::tray::TrayIconBuilder::new()
                .icon(icon)
                .tooltip("kimi-pet")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "settings" => spawn_show_settings(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        spawn_show_settings(tray.app_handle());
                    }
                })
                .build(app)?;

            // 贴边自动隐藏：监听主窗口的移动/悬停事件
            if let Some(w) = app.get_webview_window("main") {
                edge_hide::attach(&w);
            }

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(kimi::run(handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running kimi-pet");
}

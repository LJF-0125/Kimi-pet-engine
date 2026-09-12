//! 自动更新：定期检查 GitHub Releases 上的 latest.json，发现新版本弹原生对话框，
//! 用户确认后下载、验签、覆盖安装并重启。
//! 公钥配置在 tauri.conf.json，私钥在 CI Secrets（TAURI_SIGNING_PRIVATE_KEY）。

use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_updater::UpdaterExt;

/// 检查间隔：interval 首次 tick 立即触发，即启动时查一次，之后每 6 小时一次
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut timer = tokio::time::interval(CHECK_INTERVAL);
        loop {
            timer.tick().await;
            check_and_prompt(&app).await;
        }
    });
}

async fn check_and_prompt(app: &AppHandle) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[kimi-pet] 更新器不可用：{e}");
            return;
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            eprintln!("[kimi-pet] 发现新版本：{}", update.version);
            prompt_install(app.clone(), update.version.clone());
        }
        Ok(None) => {}
        Err(e) => eprintln!("[kimi-pet] 检查更新失败：{e}"),
    }
}

fn prompt_install(app: AppHandle, version: String) {
    app.dialog()
        .message(format!(
            "发现新版本 v{version}，现在更新？\n将自动下载安装并重启桌宠。"
        ))
        .title("kimi-pet 更新")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "立即更新".to_string(),
            "以后再说".to_string(),
        ))
        .show(move |ok| {
            if ok {
                tauri::async_runtime::spawn(install_and_restart(app));
            }
        });
}

/// 确认后重新拉取 Update 对象再下载，避免把 Update 移进对话框闭包。
async fn install_and_restart(app: AppHandle) {
    let Ok(updater) = app.updater() else {
        return;
    };
    let update = match updater.check().await {
        Ok(Some(u)) => u,
        // 用户犹豫期间新版被撤下，当作无事发生
        Ok(None) => return,
        Err(e) => {
            show_error(&app, &format!("检查更新失败：{e}"));
            return;
        }
    };
    if let Err(e) = update.download_and_install(|_, _| {}, || {}).await {
        show_error(&app, &format!("更新失败：{e}"));
        return;
    }
    app.restart();
}

/// 设置窗口的「检查更新」按钮：手动触发一次检查，结果返回给前端展示；
/// 发现新版本时仍走原生确认弹窗。
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<String, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            eprintln!("[kimi-pet] 手动检查发现新版本：{version}");
            prompt_install(app, version.clone());
            Ok(format!("update:{version}"))
        }
        Ok(None) => Ok("latest".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn show_error(app: &AppHandle, msg: &str) {
    app.dialog()
        .message(msg.to_string())
        .title("kimi-pet 更新")
        .buttons(MessageDialogButtons::Ok)
        .show(|_| {});
}

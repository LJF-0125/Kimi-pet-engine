//! 自动更新：定期检查 GitHub Releases 上的 latest.json，发现新版本弹原生对话框，
//! 用户确认后下载、验签、覆盖安装并重启。
//! 公钥配置在 tauri.conf.json，私钥在 CI Secrets（TAURI_SIGNING_PRIVATE_KEY）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_updater::{Update, Updater, UpdaterExt};

/// 检查间隔：interval 首次 tick 立即触发，即启动时查一次，之后每 6 小时一次
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
// check 请求与更新包下载共用的超时（GitHub 网络不稳定，避免无限等）
const UPDATE_TIMEOUT: Duration = Duration::from_secs(30);
// 下载失败后重试一次的等待时长
const RETRY_DELAY: Duration = Duration::from_secs(3);

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut timer = tokio::time::interval(CHECK_INTERVAL);
        loop {
            timer.tick().await;
            check_and_prompt(&app).await;
        }
    });
}

/// 构造带超时的更新器（check 与下载共用同一超时）。
fn build_updater(app: &AppHandle) -> tauri_plugin_updater::Result<Updater> {
    app.updater_builder().timeout(UPDATE_TIMEOUT).build()
}

/// 检查更新，最多 3 次：失败后分别等 2 秒、4 秒再试，每次失败打日志。
async fn check_with_retry(updater: &Updater) -> tauri_plugin_updater::Result<Option<Update>> {
    let mut delay = Duration::from_secs(2);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match updater.check().await {
            Ok(found) => return Ok(found),
            Err(e) => {
                eprintln!("[kimi-pet] 检查更新失败（第 {attempt}/3 次）：{e}");
                if attempt >= 3 {
                    return Err(e);
                }
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
}

async fn check_and_prompt(app: &AppHandle) {
    let updater = match build_updater(app) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("[kimi-pet] 更新器不可用：{e}");
            return;
        }
    };
    match check_with_retry(&updater).await {
        Ok(Some(update)) => {
            eprintln!("[kimi-pet] 发现新版本：{}", update.version);
            prompt_install(app.clone(), update);
        }
        Ok(None) => {}
        // 定时检查最终失败保持静默（日志已打），不弹窗打扰
        Err(e) => eprintln!("[kimi-pet] 检查更新失败（已重试 3 次）：{e}"),
    }
}

/// 弹原生确认框；用户点「立即更新」后用已拿到的 Update 对象直接下载安装。
/// Update 实现了 Resource（Send + Sync），可以安全移进异步任务，无需重新 check。
fn prompt_install(app: AppHandle, update: Update) {
    let version = update.version.clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
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
            let _ = tx.send(ok);
        });
    tauri::async_runtime::spawn(async move {
        // 用户取消或对话框异常关闭时 rx 拿不到 true，直接丢弃
        if let Ok(true) = rx.await {
            install_and_restart(app, update).await;
        }
    });
}

/// 直接用确认时拿到的 Update 对象下载安装（不再二次 check，避免 GitHub 抖动时点了没反应）。
async fn install_and_restart(app: AppHandle, update: Update) {
    // 先给即时反馈：下载需要一段时间，避免用户以为点了没反应
    app.dialog()
        .message(format!(
            "正在下载 v{}……\n完成后将自动重启桌宠。",
            update.version
        ))
        .title("kimi-pet 更新")
        .buttons(MessageDialogButtons::Ok)
        .show(|_| {});

    // 累计每跨过一次 1MB 边界打一次下载进度日志
    let downloaded = Arc::new(AtomicU64::new(0));
    for attempt in 1..=2 {
        let downloaded = Arc::clone(&downloaded);
        let result = update
            .download_and_install(
                move |chunk, total| {
                    let prev = downloaded.fetch_add(chunk as u64, Ordering::Relaxed);
                    let cur = prev + chunk as u64;
                    if cur / (1024 * 1024) != prev / (1024 * 1024) {
                        match total {
                            Some(t) => eprintln!(
                                "[kimi-pet] 更新下载中：{:.0}MB / {:.0}MB",
                                cur as f64 / 1024.0 / 1024.0,
                                t as f64 / 1024.0 / 1024.0
                            ),
                            None => eprintln!(
                                "[kimi-pet] 更新下载中：{:.0}MB",
                                cur as f64 / 1024.0 / 1024.0
                            ),
                        }
                    }
                },
                || {},
            )
            .await;
        match result {
            // Windows 上安装成功会退出进程；其他平台继续走到 restart
            Ok(()) => app.restart(),
            Err(e) if attempt == 1 => {
                eprintln!("[kimi-pet] 下载更新失败，{RETRY_DELAY:?} 后重试一次：{e}");
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(e) => {
                show_error(&app, &format!("更新失败：{e}"));
                return;
            }
        }
    }
}

/// 设置窗口的「检查更新」按钮：手动触发一次检查，结果返回给前端展示；
/// 发现新版本时仍走原生确认弹窗。
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<String, String> {
    let updater = build_updater(&app).map_err(|e| e.to_string())?;
    match check_with_retry(&updater).await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            eprintln!("[kimi-pet] 手动检查发现新版本：{version}");
            prompt_install(app, update);
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

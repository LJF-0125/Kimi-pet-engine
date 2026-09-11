//! 「跟随 Kimi Code 启动」：把启动桌宠的 hook 写入/移出官方配置 `~/.kimi-code/config.toml`。
//!
//! 勾选状态不另存，以配置里是否存在我们的 hook 条目为准。
//! 只动 `[[hooks]]` 中 command 含 "kimi-pet" 的条目，其余配置（含注释格式）原样保留。
//!
//! 写入的 hook：
//! ```toml
//! [[hooks]]
//! event = "SessionStart"
//! command = 'start "" "C:\\...\\kimi-pet.exe"'   # macOS/Linux 为 nohup 后台启动
//! ```

use std::fs;
use std::path::PathBuf;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

/// 识别我们写入的 hook 条目（command 里带 exe 名）。
const HOOK_MARKER: &str = "kimi-pet";

fn config_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".kimi-code").join("config.toml"))
}

/// 启动桌宠的 shell 命令。hook 有超时且按进程组清理，必须让桌宠脱离 hook 进程独立存活。
#[cfg(windows)]
fn launch_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("start \"\" \"{}\"", exe.display()))
}

#[cfg(not(windows))]
fn launch_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("nohup \"{}\" >/dev/null 2>&1 &", exe.display()))
}

fn is_our_hook(t: &Table) -> bool {
    t.get("command")
        .and_then(|v| v.as_str())
        .is_some_and(|c| c.contains(HOOK_MARKER))
}

fn hook_exists(doc: &DocumentMut) -> bool {
    doc.as_table()
        .get("hooks")
        .and_then(|h| h.as_array_of_tables())
        .is_some_and(|aot| aot.iter().any(is_our_hook))
}

/// 设置窗口勾选框的回显状态。
#[tauri::command]
pub fn get_launch_hook() -> bool {
    let Some(path) = config_path() else { return false };
    let Ok(text) = fs::read_to_string(path) else { return false };
    let Ok(doc) = text.parse::<DocumentMut>() else { return false };
    hook_exists(&doc)
}

/// 勾选 / 取消勾选：写入或移除 hook 条目。失败时返回错误信息给前端提示。
#[tauri::command]
pub fn set_launch_hook(enabled: bool) -> Result<(), String> {
    let path = config_path().ok_or("找不到用户目录")?;
    let text = fs::read_to_string(&path).unwrap_or_default();
    let mut doc = if text.trim().is_empty() {
        DocumentMut::new()
    } else {
        text.parse::<DocumentMut>()
            .map_err(|e| format!("config.toml 解析失败，请检查原有语法：{e}"))?
    };

    if !doc.as_table().contains_key("hooks") {
        doc["hooks"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    // 已有 hooks 但不是 [[hooks]] 表数组（比如写成了 hooks = ...），不擅自覆盖
    let aot = doc["hooks"]
        .as_array_of_tables_mut()
        .ok_or("config.toml 里 hooks 字段不是表数组，未改动")?;

    // 先清掉旧条目（exe 可能已移动），需要时再按当前路径重写
    let stale: Vec<usize> = aot
        .iter()
        .enumerate()
        .filter(|(_, t)| is_our_hook(t))
        .map(|(i, _)| i)
        .collect();
    for i in stale.into_iter().rev() {
        aot.remove(i);
    }

    if enabled {
        let command = launch_command().ok_or("获取 exe 路径失败")?;
        let mut t = Table::new();
        t["event"] = value("SessionStart");
        t["command"] = value(command);
        aot.push(t);
    } else if aot.is_empty() {
        // 没有剩余条目时连 hooks 键一起清掉，保持配置干净
        doc.as_table_mut().remove("hooks");
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败：{e}"))?;
    }
    fs::write(&path, doc.to_string()).map_err(|e| format!("写入 config.toml 失败：{e}"))?;
    Ok(())
}

//! 贴边自动隐藏：把窗口拖到屏幕左/右/上边缘松手后自动滑出，只留一小条在边缘；
//! 鼠标悬停露出的一小条时滑回完整位置（不自动收回，再次拖到边缘才重新隐藏）。
//! 注意：Wayland 下 Tauri 无法设置窗口位置，此功能静默失效（Windows / macOS / X11 正常）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{Manager, PhysicalPosition, WebviewWindow, WindowEvent};

/// 贴边判定阈值（物理像素）：窗口边缘与屏幕边缘间距在此范围内视为贴边
const SNAP_GAP: i32 = 8;
/// 隐藏后留在屏幕内的比例：露出窗口宽/高的 1/4
const SLIVER_DIV: i32 = 4;
/// 拖动结束后的防抖延迟：松手超过这个时间没有再移动才触发隐藏
const SETTLE_MS: u64 = 350;

#[derive(Clone, Copy, PartialEq)]
enum Edge {
    Left,
    Right,
    Top,
}

struct EdgeHideState {
    /// 隐藏方向 + 隐藏前的完整位置；None 表示当前未隐藏
    hidden: Option<(Edge, PhysicalPosition<i32>)>,
    /// 最后一次 Moved 事件时间，用于防抖
    last_move: Option<Instant>,
    /// 滑动动画进行中（期间的 Moved 事件忽略，避免动画与拖动打架）
    animating: bool,
}

static STATE: Mutex<EdgeHideState> = Mutex::new(EdgeHideState {
    hidden: None,
    last_move: None,
    animating: false,
});

static ENABLED: AtomicBool = AtomicBool::new(true);

#[tauri::command]
pub fn get_edge_hide() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 设置窗口的开关；关闭时若正处于隐藏状态，先把窗口滑回完整位置。
#[tauri::command]
pub fn set_edge_hide(app: tauri::AppHandle, enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled {
        if let Some(w) = app.get_webview_window("main") {
            reveal(&w);
        }
    }
}

/// 给主窗口注册移动 / 悬停事件处理（在 setup 里调用一次）。
pub fn attach(window: &WebviewWindow) {
    let w = window.clone();
    window.on_window_event(move |event| match event {
        WindowEvent::Moved(_) => on_moved(&w),
        WindowEvent::CursorEntered { .. } => {
            if STATE.lock().unwrap().hidden.is_some() {
                reveal(&w);
            }
        }
        _ => {}
    });
}

/// 拖动中会持续收到 Moved：记录时间并起防抖线程，松手 SETTLE_MS 后仍在边缘才隐藏。
fn on_moved(window: &WebviewWindow) {
    {
        let mut s = STATE.lock().unwrap();
        if s.animating {
            return;
        }
        s.last_move = Some(Instant::now());
    }
    let w = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(SETTLE_MS));
        {
            let s = STATE.lock().unwrap();
            let settled = s
                .last_move
                .map(|t| t.elapsed() >= Duration::from_millis(SETTLE_MS))
                .unwrap_or(false);
            if s.animating || s.hidden.is_some() || !settled || !ENABLED.load(Ordering::Relaxed) {
                return;
            }
        }
        if let Some((edge, target)) = hide_target(&w) {
            if let Ok(full) = w.outer_position() {
                animate(&w, target);
                STATE.lock().unwrap().hidden = Some((edge, full));
            }
        }
    });
}

/// 悬停露出的一小条：滑回隐藏前的完整位置（不自动收回）。
fn reveal(window: &WebviewWindow) {
    let full = {
        let mut s = STATE.lock().unwrap();
        match s.hidden.take() {
            Some((_, pos)) => pos,
            None => return,
        }
    };
    let w = window.clone();
    std::thread::spawn(move || animate(&w, full));
}

/// 窗口贴在左/右/上边缘时，计算隐藏后的目标位置（只留 1/4 在屏内）。
fn hide_target(window: &WebviewWindow) -> Option<(Edge, PhysicalPosition<i32>)> {
    let mon = window.current_monitor().ok().flatten()?;
    let mp = *mon.position();
    let ms = *mon.size();
    let wp = window.outer_position().ok()?;
    let ws = window.outer_size().ok()?;
    let (mw, mh) = (ms.width as i32, ms.height as i32);
    let (ww, wh) = (ws.width as i32, ws.height as i32);
    if ww >= mw || wh >= mh {
        return None;
    }

    let gap_left = wp.x - mp.x;
    let gap_right = (mp.x + mw) - (wp.x + ww);
    let gap_top = wp.y - mp.y;
    let min = gap_left.min(gap_right).min(gap_top);
    if min > SNAP_GAP {
        return None;
    }

    let clamp = |v: i32, lo: i32, hi: i32| v.max(lo).min(hi.max(lo));
    if min == gap_left {
        let y = clamp(wp.y, mp.y, mp.y + mh - wh);
        Some((Edge::Left, PhysicalPosition::new(mp.x - (ww - ww / SLIVER_DIV), y)))
    } else if min == gap_right {
        let y = clamp(wp.y, mp.y, mp.y + mh - wh);
        Some((Edge::Right, PhysicalPosition::new(mp.x + mw - ww / SLIVER_DIV, y)))
    } else {
        let x = clamp(wp.x, mp.x, mp.x + mw - ww);
        Some((Edge::Top, PhysicalPosition::new(x, mp.y - (wh - wh / SLIVER_DIV))))
    }
}

/// 10 步线性插值滑动到目标位置；调用方需在非事件循环线程上调用（会 sleep）。
fn animate(window: &WebviewWindow, target: PhysicalPosition<i32>) {
    let Ok(from) = window.outer_position() else { return };
    STATE.lock().unwrap().animating = true;
    const STEPS: i32 = 10;
    for i in 1..=STEPS {
        let x = from.x + (target.x - from.x) * i / STEPS;
        let y = from.y + (target.y - from.y) * i / STEPS;
        let _ = window.set_position(PhysicalPosition::new(x, y));
        std::thread::sleep(Duration::from_millis(16));
    }
    STATE.lock().unwrap().animating = false;
}

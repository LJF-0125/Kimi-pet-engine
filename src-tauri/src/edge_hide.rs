//! 贴边自动隐藏：把窗口拖到屏幕左/右/上边缘松手后自动滑出，鼠标悬停露出部分时滑回
//! （不自动收回，再次拖到边缘才重新隐藏）。
//! 三种模式（前端 set_hide_mode 同步）：
//! - off：不隐藏
//! - sliver：只留窗口宽/高的 1/4 在边缘
//! - head：只露头——前端按隐藏方向旋转画面（左 90°CW / 右 90°CCW / 顶 180°），
//!   让身体朝向屏幕外被边框自然挡住，窗口按旋转后的头框贴边；无头框时回退 sliver
//! 注意：Wayland 下 Tauri 无法设置窗口位置，此功能静默失效（Windows / macOS / X11 正常）。

use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager, PhysicalPosition, WebviewWindow, WindowEvent};

/// 贴边判定阈值（物理像素）：窗口边缘与屏幕边缘间距在此范围内视为贴边
const SNAP_GAP: i32 = 8;
/// sliver 模式隐藏后留在屏幕内的比例：露出窗口宽/高的 1/4
const SLIVER_DIV: i32 = 4;
/// head 模式头部埋进屏幕边缘的比例（0.15 = 头部 85% 露出）
const BURY: f64 = 0.15;
/// 拖动结束后的防抖延迟：松手超过这个时间没有再移动才触发隐藏
const SETTLE_MS: u64 = 350;

#[derive(Clone, Copy, PartialEq)]
enum HideMode {
    Off,
    Sliver,
    Head,
}

#[derive(Clone, Copy, PartialEq)]
enum Edge {
    Left,
    Right,
    Top,
}

/// 当前显示中 GIF 的头框（窗口相对坐标 0~1），由前端随状态切换同步
#[derive(serde::Deserialize, Clone, Copy)]
pub struct RelRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
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

static MODE: Mutex<HideMode> = Mutex::new(HideMode::Sliver);
static HEAD_RECT: Mutex<Option<RelRect>> = Mutex::new(None);

/// 设置隐藏模式（off / sliver / head，非法值按 sliver）；切到 off 时若隐藏中先滑回。
#[tauri::command]
pub fn set_hide_mode(app: tauri::AppHandle, mode: String) {
    let mode = match mode.as_str() {
        "off" => HideMode::Off,
        "head" => HideMode::Head,
        _ => HideMode::Sliver,
    };
    *MODE.lock().unwrap() = mode;
    if mode == HideMode::Off {
        if let Some(w) = app.get_webview_window("main") {
            reveal(&w);
        }
    }
}

/// 前端随当前 GIF 状态同步头框；None 表示当前 GIF 没有头框（head 模式回退 sliver）
#[tauri::command]
pub fn set_head_rect(rect: Option<RelRect>) {
    *HEAD_RECT.lock().unwrap() = rect;
}

/// 给主窗口注册移动事件处理（在 setup 里调用一次）。
/// 悬停露出由前端 mouseenter 触发 reveal_if_hidden（tauri::WindowEvent 没有光标事件）。
pub fn attach(window: &WebviewWindow) {
    let w = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::Moved(_) = event {
            on_moved(&w);
        }
    });
}

/// 前端 mouseenter 时调用：隐藏中则滑回完整位置（不自动收回）。
#[tauri::command]
pub fn reveal_if_hidden(app: tauri::AppHandle) {
    if STATE.lock().unwrap().hidden.is_none() {
        return;
    }
    if let Some(w) = app.get_webview_window("main") {
        reveal(&w);
    }
}

/// 满足条件就把窗口藏进边缘：未隐藏、非动画中、非拖动中、模式开启、当前位置在贴边阈值内。
fn try_hide(w: &WebviewWindow) {
    {
        let s = STATE.lock().unwrap();
        // 拖动会产生持续的 Moved 事件：最近 300ms 内还有 Moved 视为拖动中，不打断
        let dragging = s
            .last_move
            .map(|t| t.elapsed() < Duration::from_millis(300))
            .unwrap_or(false);
        if s.animating || s.hidden.is_some() || dragging || *MODE.lock().unwrap() == HideMode::Off {
            return;
        }
    }
    if let Some((edge, target, rotate)) = hide_target(w) {
        if let Ok(full) = w.outer_position() {
            let _ = w.emit(
                "edge-hide",
                serde_json::json!({ "hidden": true, "rotate": rotate }),
            );
            animate(w, target);
            STATE.lock().unwrap().hidden = Some((edge, full));
        }
    }
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
            if !settled {
                return;
            }
        }
        try_hide(&w);
    });
}

/// 前端 mouseleave 防抖后调用：唤回后没拖走、还停在边缘 → 自动重新藏进去。
/// 重试几次：唤回动画可能还在播（animating 时 try_hide 会跳过）。
#[tauri::command]
pub fn try_rehide(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        std::thread::spawn(move || {
            for _ in 0..3 {
                try_hide(&w);
                if STATE.lock().unwrap().hidden.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(400));
            }
        });
    }
}

/// 悬停露出部分：滑回隐藏前的完整位置（不自动收回）。
fn reveal(window: &WebviewWindow) {
    let full = {
        let mut s = STATE.lock().unwrap();
        match s.hidden.take() {
            Some((_, pos)) => pos,
            None => return,
        }
    };
    let _ = window.emit("edge-hide", serde_json::json!({ "hidden": false }));
    let w = window.clone();
    std::thread::spawn(move || animate(&w, full));
}

/// 窗口贴在左/右/上边缘时，计算隐藏后的目标位置。
/// 返回 (方向, 目标位置, 前端应旋转的角度)；None 表示不贴边。
fn hide_target(window: &WebviewWindow) -> Option<(Edge, PhysicalPosition<i32>, i32)> {
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

    // head 模式且有头框：画面绕窗口中心旋转，让身体朝向屏幕外（左 90°CW / 右 90°CCW /
    // 顶 180°），身体被边框挡住；再按旋转后的头框贴边（85% 露出）
    if *MODE.lock().unwrap() == HideMode::Head {
        if let Some(r) = *HEAD_RECT.lock().unwrap() {
            // 把头框映射到旋转后的窗口坐标（窗口是正方形，绕中心转 90/180 仍落在窗口内）
            let (r, rotate) = if min == gap_left {
                // rotate(90deg CW): (x,y) → (1-y, x)，宽高互换
                (
                    RelRect { x: 1.0 - r.y - r.h, y: r.x, w: r.h, h: r.w },
                    90,
                )
            } else if min == gap_right {
                // rotate(270deg CW): (x,y) → (y, 1-x)，宽高互换
                (
                    RelRect { x: r.y, y: 1.0 - r.x - r.w, w: r.h, h: r.w },
                    270,
                )
            } else {
                // rotate(180deg): (x,y) → (1-x, 1-y)
                (
                    RelRect { x: 1.0 - r.x - r.w, y: 1.0 - r.y - r.h, w: r.w, h: r.h },
                    180,
                )
            };
            let (hx, hy) = ((r.x * ww as f64) as i32, (r.y * wh as f64) as i32);
            let (hw, hh) = ((r.w * ww as f64) as i32, (r.h * wh as f64) as i32);
            if min == gap_left {
                let tx = mp.x - hx - (hw as f64 * BURY) as i32;
                let ty = clamp(wp.y, mp.y - hy, mp.y + mh - hy - hh);
                return Some((Edge::Left, PhysicalPosition::new(tx, ty), rotate));
            } else if min == gap_right {
                let tx = mp.x + mw - hx - (hw as f64 * (1.0 - BURY)) as i32;
                let ty = clamp(wp.y, mp.y - hy, mp.y + mh - hy - hh);
                return Some((Edge::Right, PhysicalPosition::new(tx, ty), rotate));
            } else {
                let ty = mp.y - hy - (hh as f64 * BURY) as i32;
                let tx = clamp(wp.x, mp.x - hx, mp.x + mw - hx - hw);
                return Some((Edge::Top, PhysicalPosition::new(tx, ty), rotate));
            }
        }
    }

    // sliver 模式（或 head 无头框回退）：露出窗口宽/高的 1/4，不旋转
    if min == gap_left {
        let y = clamp(wp.y, mp.y, mp.y + mh - wh);
        Some((
            Edge::Left,
            PhysicalPosition::new(mp.x - (ww - ww / SLIVER_DIV), y),
            0,
        ))
    } else if min == gap_right {
        let y = clamp(wp.y, mp.y, mp.y + mh - wh);
        Some((
            Edge::Right,
            PhysicalPosition::new(mp.x + mw - ww / SLIVER_DIV, y),
            0,
        ))
    } else {
        let x = clamp(wp.x, mp.x, mp.x + mw - ww);
        Some((
            Edge::Top,
            PhysicalPosition::new(x, mp.y - (wh - wh / SLIVER_DIV)),
            0,
        ))
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

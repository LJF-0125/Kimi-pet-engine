//! 连接本机 `kimi web` 服务（REST + WebSocket），把会话事件映射成桌宠状态。
//!
//! 状态（发给前端的 `pet-state` 事件，payload 为字符串）：
//! - `thinking`  思考中：轮次开始 / 推理中 / 工具调用中
//! - `answering` 编辑中：正在流式输出正文
//! - `approval`  待审核：等待用户处理审批或提问
//! - `idle`      空闲中：无活动轮次（含一轮结束），也是初始状态
//! - `offline`   kimi web 服务不在线
//!
//! 多会话并发时按会话聚合：任一待审核 > 任一编辑中 > 任一思考中 > 全空闲。
//!
//! 「Kimi Code 退出后自动关闭」（设置项，默认关）：本次运行连上过服务后，
//! 持续失联超过 AUTO_QUIT_AFTER 视为 Kimi Code 已退出，自动结束进程；
//! 收到服务端优雅关停的关闭帧（reason 为 'server shutting down'）则立即退出。

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::fs;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
};

const BASE_PORT: u16 = 58627;
const MAX_PORT_TRIES: u16 = 100; // 官方文档：端口占用时服务至多递增重试 100 次
const RETRY_DELAY: Duration = Duration::from_secs(5);
// 断线宽限：10 秒内保持原状态不灰化，超过仍未恢复才置为 offline
const OFFLINE_GRACE: Duration = Duration::from_secs(10);
// 自动关闭：连上过服务后持续失联超过该时长视为 Kimi Code 已退出（2 分钟，容忍服务短暂重启）
const AUTO_QUIT_AFTER: Duration = Duration::from_secs(120);
// REST 轮询间隔：用 busy / pending_interaction 校准 WS 事件推断出的状态
const POLL_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PetState {
    Thinking,
    Answering,
    Approval,
    Idle,
    Offline,
}

impl PetState {
    fn as_str(self) -> &'static str {
        match self {
            PetState::Thinking => "thinking",
            PetState::Answering => "answering",
            PetState::Approval => "approval",
            PetState::Idle => "idle",
            PetState::Offline => "offline",
        }
    }
}

/// 单个会话的忙态（离线/空闲不入表，turn 结束即从表中移除）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SessionState {
    Thinking,
    Answering,
    Approval,
}

/// 最近一次对外呈现的状态，供前端启动时主动拉取（事件不重放，listen 注册前会丢）。
static CURRENT_STATE: OnceLock<Mutex<&'static str>> = OnceLock::new();

fn state_cell() -> &'static Mutex<&'static str> {
    CURRENT_STATE.get_or_init(|| Mutex::new(PetState::Offline.as_str()))
}

/// 前端 `get_state` command 用：读取当前状态。
pub fn current_state() -> &'static str {
    *state_cell().lock().unwrap()
}

/// 「Kimi Code 退出后自动关闭」开关：勾选状态存前端 IndexedDB，启动/变更时由主窗口推送。
static AUTO_QUIT: AtomicBool = AtomicBool::new(false);
/// 本次运行是否连上过服务：没连上过不自动退出（用户可能只是单独开着桌宠，服务稍后才会起）。
static CONNECTED_ONCE: AtomicBool = AtomicBool::new(false);

#[tauri::command]
pub fn set_auto_quit(enabled: bool) {
    AUTO_QUIT.store(enabled, Ordering::Relaxed);
}

/// 自动关闭是否生效：开关打开且本次运行连上过服务。
fn auto_quit_armed() -> bool {
    AUTO_QUIT.load(Ordering::Relaxed) && CONNECTED_ONCE.load(Ordering::Relaxed)
}

fn set_state(app: &AppHandle, state: PetState) {
    let mut cur = state_cell().lock().unwrap();
    if *cur != state.as_str() {
        *cur = state.as_str();
        let _ = app.emit("pet-state", state.as_str());
    }
}

/// 读取 `~/.kimi-code/server.token`（kimi web 的 bearer token）。
fn read_token() -> Option<String> {
    let home = dirs::home_dir()?;
    let token = fs::read_to_string(home.join(".kimi-code").join("server.token")).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// 读取 `~/.kimi-code/server/instances/*.json` 中各实例自报的端口。
/// 新版服务的端口可能落在默认扫描范围之外，会把实际端口写进实例清单；
/// 文件可能来自已退出的实例，返回的端口仍需 healthz 验证。
fn instance_ports() -> Vec<u16> {
    let mut ports = Vec::new();
    let Some(dir) = dirs::home_dir().map(|h| h.join(".kimi-code").join("server").join("instances")) else {
        return ports;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return ports;
    };
    for entry in entries.flatten() {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(port) = v.get("port").and_then(|p| p.as_u64()).and_then(|p| u16::try_from(p).ok()) {
            ports.push(port);
        }
    }
    ports
}

async fn healthz_ok(client: &reqwest::Client, port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/api/v1/healthz");
    matches!(client.get(&url).send().await, Ok(resp) if resp.status().is_success())
}

/// 发现本地 kimi web 服务：先按实例清单（服务自报端口）探测，再从默认端口起扫描。
async fn find_port() -> Option<u16> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .ok()?;
    for port in instance_ports() {
        if healthz_ok(&client, port).await {
            return Some(port);
        }
    }
    for port in BASE_PORT..BASE_PORT + MAX_PORT_TRIES {
        if healthz_ok(&client, port).await {
            return Some(port);
        }
    }
    None
}

/// 拼出 web UI 地址，供双击打开。
pub async fn web_url() -> Option<String> {
    let token = read_token()?;
    let port = find_port().await?;
    Some(format!("http://127.0.0.1:{port}/#token={token}"))
}

/// 拉取会话 id 列表用于订阅（只拉第一页，超 100 个会话的极端情况会漏订阅，可接受）。
async fn list_session_ids(client: &reqwest::Client, port: u16, token: &str) -> Vec<String> {
    fetch_sessions(client, port, token)
        .await
        .into_iter()
        .map(|s| s.id)
        .collect()
}

/// `agent.status.updated` 的 phase.kind → 单会话忙态；None 表示不影响状态。
/// 未知的 kind 打日志，便于发现新枚举值。
fn map_phase(phase: &Value) -> Option<SessionState> {
    match phase.get("kind").and_then(|v| v.as_str())? {
        // 思考中：agent 运行中、工具调用中
        "running" | "tool_call" => Some(SessionState::Thinking),
        // 流式输出：思考流算思考，正文流算回答
        "streaming" => {
            if phase.get("stream").and_then(|v| v.as_str()) == Some("thinking") {
                Some(SessionState::Thinking)
            } else {
                Some(SessionState::Answering)
            }
        }
        // 轮次结束 / 空闲
        "idle" | "done" | "completed" => None,
        other => {
            eprintln!("[kimi-pet] 未知 phase kind：{other}");
            None
        }
    }
}

struct SessionInfo {
    id: String,
    busy: bool,
    pending: String,
}

/// 拉取会话列表（含忙态/待处理交互），用于订阅和状态校准。
async fn fetch_sessions(client: &reqwest::Client, port: u16, token: &str) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    let url = format!("http://127.0.0.1:{port}/api/v1/sessions?page_size=100");
    if let Ok(resp) = client.get(&url).bearer_auth(token).send().await {
        if let Ok(envelope) = resp.json::<Value>().await {
            let data = envelope.get("data").cloned().unwrap_or(Value::Null);
            let items = data
                .get("items")
                .and_then(|v| v.as_array())
                .or_else(|| data.as_array());
            if let Some(items) = items {
                for it in items {
                    let Some(id) = it.get("id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    out.push(SessionInfo {
                        id: id.to_string(),
                        busy: it.get("busy").and_then(|v| v.as_bool()).unwrap_or(false),
                        pending: it
                            .get("pending_interaction")
                            .and_then(|v| v.as_str())
                            .unwrap_or("none")
                            .to_string(),
                    });
                }
            }
        }
    }
    out
}

/// 用 REST 权威状态校准忙态表：待处理交互 > 忙碌 > 空闲。返回状态是否有变化。
fn reconcile(sessions: &mut HashMap<String, SessionState>, infos: &[SessionInfo]) -> bool {
    let mut changed = false;
    for info in infos {
        let prev = sessions.get(&info.id).copied();
        let next = if !info.pending.is_empty() && info.pending != "none" {
            Some(SessionState::Approval)
        } else if info.busy {
            // 不覆盖 WS 已给出的更细状态（Answering 等）
            Some(prev.filter(|s| *s != SessionState::Approval).unwrap_or(SessionState::Thinking))
        } else {
            None
        };
        if prev != next {
            changed = true;
            match next {
                Some(s) => sessions.insert(info.id.clone(), s),
                None => sessions.remove(&info.id),
            };
        }
    }
    changed
}

/// 聚合所有会话的忙态得到桌宠状态。
fn aggregate(sessions: &HashMap<String, SessionState>) -> PetState {
    let mut result = PetState::Idle;
    for s in sessions.values() {
        match s {
            SessionState::Approval => return PetState::Approval,
            SessionState::Answering => result = PetState::Answering,
            SessionState::Thinking => {
                if result == PetState::Idle {
                    result = PetState::Thinking;
                }
            }
        }
    }
    result
}

/// 记录断线时刻：宽限期内保持原状态，超过 OFFLINE_GRACE 仍未恢复才置为离线。
/// 自动关闭生效时，持续失联超过 AUTO_QUIT_AFTER 视为 Kimi Code 已退出，结束进程。
fn mark_disconnected(app: &AppHandle, since: &mut Option<Instant>) {
    let start = since.get_or_insert_with(Instant::now);
    if start.elapsed() >= OFFLINE_GRACE {
        set_state(app, PetState::Offline);
    }
    if auto_quit_armed() && start.elapsed() >= AUTO_QUIT_AFTER {
        eprintln!("[kimi-pet] 服务持续失联超过 {AUTO_QUIT_AFTER:?}，按「自动关闭」设置退出");
        app.exit(0);
    }
}

/// 主循环：发现服务 → 连 WebSocket → 消费事件；断线自动重连。
pub async fn run(app: AppHandle) {
    let mut disconnected_since: Option<Instant> = None;
    loop {
        // 发现服务（宽限期内保持原状态，超过 OFFLINE_GRACE 仍未恢复才灰化）
        let (token, port) = loop {
            match (read_token(), find_port().await) {
                (Some(token), Some(port)) => break (token, port),
                (None, _) => {
                    eprintln!("[kimi-pet] 未找到 ~/.kimi-code/server.token，{RETRY_DELAY:?} 后重试");
                    mark_disconnected(&app, &mut disconnected_since);
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                (Some(_), None) => {
                    eprintln!("[kimi-pet] 未发现 kimi web 服务（{BASE_PORT} 起 {MAX_PORT_TRIES} 个端口均无响应），{RETRY_DELAY:?} 后重试");
                    mark_disconnected(&app, &mut disconnected_since);
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        };

        let url = format!("ws://127.0.0.1:{port}/api/v1/ws");
        let Ok(mut request) = url.clone().into_client_request() else {
            eprintln!("[kimi-pet] 构造 WS 请求失败：{url}");
            tokio::time::sleep(RETRY_DELAY).await;
            continue;
        };
        match HeaderValue::from_str(&format!("kimi-code.bearer.{token}")) {
            Ok(value) => {
                request.headers_mut().insert("Sec-WebSocket-Protocol", value);
            }
            Err(e) => {
                eprintln!("[kimi-pet] token 含非法 header 字符，无法鉴权：{e}");
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        }

        let ws = match connect_async(request).await {
            Ok((stream, _)) => stream,
            Err(e) => {
                eprintln!("[kimi-pet] WS 连接失败（{url}）：{e}");
                mark_disconnected(&app, &mut disconnected_since);
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        eprintln!("[kimi-pet] 已连接 kimi web（端口 {port}）");
        disconnected_since = None;
        CONNECTED_ONCE.store(true, Ordering::Relaxed);

        // 订阅当前所有会话的事件
        let client = reqwest::Client::new();
        let ids = list_session_ids(&client, port, &token).await;
        let (mut ws_write, mut ws_read) = ws.split();
        if ids.is_empty() {
            eprintln!("[kimi-pet] 会话列表为空，仅接收全局事件");
        } else {
            let frame = json!({
                "type": "subscribe",
                "id": "sub-0",
                "payload": { "session_ids": ids }
            });
            if let Err(e) = ws_write.send(Message::text(frame.to_string())).await {
                eprintln!("[kimi-pet] 订阅帧发送失败：{e}");
            }
        }
        let mut subscribed: HashSet<String> = ids.into_iter().collect();

        let mut sessions: HashMap<String, SessionState> = HashMap::new();
        set_state(&app, PetState::Idle);
        let mut poll = tokio::time::interval(POLL_INTERVAL);

        loop {
            tokio::select! {
                msg = ws_read.next() => {
                    let text = match msg {
                        Some(Ok(Message::Text(t))) => t,
                        // 服务器优雅关停会给所有连接发 reason 为 'server shutting down' 的关闭帧：
                        // 语义明确（区别于心跳超时踢线），自动关闭生效时立即退出
                        Some(Ok(Message::Close(Some(frame))))
                            if frame.reason.as_str() == "server shutting down" && auto_quit_armed() =>
                        {
                            eprintln!("[kimi-pet] 收到服务关停信号，按「自动关闭」设置退出");
                            app.exit(0);
                            break;
                        }
                        // None / Close / Err 都视为连接断开
                        None | Some(Ok(Message::Close(_))) | Some(Err(_)) => break,
                        Some(Ok(_)) => continue,
                    };
                    let Ok(event) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    let Some(event_type) = event.get("type").and_then(|v| v.as_str()) else {
                        continue;
                    };

                    // 应用层心跳：必须回 pong，否则服务端 30 秒左右掐线
                    if event_type == "ping" {
                        let frame = json!({
                            "type": "pong",
                            "payload": event.get("payload").cloned().unwrap_or(Value::Null)
                        });
                        let _ = ws_write.send(Message::text(frame.to_string())).await;
                        continue;
                    }

                    // 全局事件：新会话创建时补订阅
                    if event_type == "event.session.created" {
                        let new_id = event
                            .pointer("/payload/sessionId")
                            .or_else(|| event.pointer("/payload/id"))
                            .or_else(|| event.pointer("/payload/session_id"))
                            .and_then(|v| v.as_str());
                        match new_id {
                            Some(id) => {
                                let frame = json!({
                                    "type": "subscribe",
                                    "payload": { "session_ids": [id] }
                                });
                                if let Err(e) = ws_write.send(Message::text(frame.to_string())).await {
                                    eprintln!("[kimi-pet] 补订阅 {id} 失败：{e}");
                                } else {
                                    subscribed.insert(id.to_string());
                                }
                            }
                            None => eprintln!("[kimi-pet] event.session.created 未解析到会话 id：{text}"),
                        }
                        continue;
                    }

                    // 会话事件：按 session_id 维护忙态表并聚合
                    let Some(session_id) = event.get("session_id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    match event_type {
                        // agent 自报阶段：状态的主要来源
                        "agent.status.updated" => {
                            if let Some(phase) = event.pointer("/payload/phase") {
                                match map_phase(phase) {
                                    Some(s) => sessions.insert(session_id.to_string(), s),
                                    None => sessions.remove(session_id),
                                };
                                set_state(&app, aggregate(&sessions));
                            }
                        }
                        // 待审核
                        "event.approval.requested" | "event.question.requested" => {
                            sessions.insert(session_id.to_string(), SessionState::Approval);
                            set_state(&app, aggregate(&sessions));
                        }
                        // 审批处理完，回到思考中
                        "event.approval.resolved" | "event.question.answered"
                        | "event.question.dismissed" => {
                            sessions.insert(session_id.to_string(), SessionState::Thinking);
                            set_state(&app, aggregate(&sessions));
                        }
                        // 轮次结束
                        "turn.ended" | "turn.step.interrupted" | "error" => {
                            sessions.remove(session_id);
                            set_state(&app, aggregate(&sessions));
                        }
                        _ => {}
                    }
                }
                _ = poll.tick() => {
                    // REST 轮询校准：兜底漏事件/卡状态；顺带补订阅新会话
                    let infos = fetch_sessions(&client, port, &token).await;
                    if reconcile(&mut sessions, &infos) {
                        set_state(&app, aggregate(&sessions));
                    }
                    let new_ids: Vec<String> = infos
                        .iter()
                        .map(|i| i.id.clone())
                        .filter(|id| !subscribed.contains(id))
                        .collect();
                    if !new_ids.is_empty() {
                        let frame = json!({
                            "type": "subscribe",
                            "payload": { "session_ids": new_ids }
                        });
                        if let Err(e) = ws_write.send(Message::text(frame.to_string())).await {
                            eprintln!("[kimi-pet] 补订阅失败：{e}");
                        } else {
                            subscribed.extend(new_ids);
                        }
                    }
                }
            }
        }

        eprintln!("[kimi-pet] WS 连接断开，{RETRY_DELAY:?} 后重连");
        mark_disconnected(&app, &mut disconnected_since);
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

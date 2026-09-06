//! 连接本机 `kimi web` 服务（REST + WebSocket），把会话事件映射成桌宠状态。
//!
//! 状态（发给前端的 `pet-state` 事件，payload 为字符串）：
//! - `thinking`  思考中：轮次开始 / 推理中 / 工具调用中
//! - `answering` 回答中：正在流式输出正文
//! - `approval`  待审核：等待用户处理审批或提问
//! - `idle`      空闲中：无活动轮次（含一轮结束），也是初始状态
//! - `offline`   kimi web 服务不在线
//!
//! 多会话并发时按会话聚合：任一待审核 > 任一回答中 > 任一思考中 > 全空闲。

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::{fs, time::Duration};
use tauri::{AppHandle, Emitter};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
};

const BASE_PORT: u16 = 58627;
const MAX_PORT_TRIES: u16 = 100; // 官方文档：端口占用时服务至多递增重试 100 次
const RETRY_DELAY: Duration = Duration::from_secs(5);

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

/// 从默认端口起探测本地 kimi web 服务（healthz 无需鉴权）。
async fn find_port() -> Option<u16> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .ok()?;
    for port in BASE_PORT..BASE_PORT + MAX_PORT_TRIES {
        let url = format!("http://127.0.0.1:{port}/api/v1/healthz");
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                return Some(port);
            }
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
    let mut ids = Vec::new();
    let url = format!("http://127.0.0.1:{port}/api/v1/sessions?page_size=100");
    if let Ok(resp) = client.get(&url).bearer_auth(token).send().await {
        if let Ok(envelope) = resp.json::<Value>().await {
            let data = envelope.get("data").cloned().unwrap_or(Value::Null);
            // 兼容两种形状：data.items 或 data 直接是数组
            let items = data
                .get("items")
                .and_then(|v| v.as_array())
                .or_else(|| data.as_array());
            if let Some(items) = items {
                for it in items {
                    if let Some(id) = it.get("id").and_then(|v| v.as_str()) {
                        ids.push(id.to_string());
                    }
                }
            }
        }
    }
    ids
}

/// 把会话事件类型映射为单会话忙态；None 表示不影响状态。
fn map_event(event_type: &str) -> Option<SessionState> {
    match event_type {
        // 思考中：轮次开始、推理流、工具调用（按主人要求工具干活也算思考）
        "turn.started" | "turn.step.started" | "thinking.delta" | "tool.call.started"
        | "tool.call.delta" | "tool.progress" | "subagent.started" => Some(SessionState::Thinking),
        // 回答中：正文流式输出
        "assistant.delta" => Some(SessionState::Answering),
        // 待审核
        "event.approval.requested" | "event.question.requested" => Some(SessionState::Approval),
        // 审批处理完，回到思考中
        "event.approval.resolved" | "event.question.answered" | "event.question.dismissed" => {
            Some(SessionState::Thinking)
        }
        _ => None,
    }
}

/// 轮次结束类事件（会话回到空闲，从忙态表移除）。
fn is_turn_over(event_type: &str) -> bool {
    matches!(event_type, "turn.ended" | "turn.step.interrupted" | "error")
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

/// 主循环：发现服务 → 连 WebSocket → 消费事件；断线自动重连。
pub async fn run(app: AppHandle) {
    loop {
        // 发现服务（每次探测失败都刷新 offline，保证启动早期 listen 未注册时状态也能被拉到）
        let (token, port) = loop {
            match (read_token(), find_port().await) {
                (Some(token), Some(port)) => break (token, port),
                (None, _) => {
                    eprintln!("[kimi-pet] 未找到 ~/.kimi-code/server.token，{RETRY_DELAY:?} 后重试");
                    set_state(&app, PetState::Offline);
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                (Some(_), None) => {
                    eprintln!("[kimi-pet] 未发现 kimi web 服务（{BASE_PORT} 起 {MAX_PORT_TRIES} 个端口均无响应），{RETRY_DELAY:?} 后重试");
                    set_state(&app, PetState::Offline);
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

        let mut ws = match connect_async(request).await {
            Ok((stream, _)) => stream,
            Err(e) => {
                eprintln!("[kimi-pet] WS 连接失败（{url}）：{e}");
                set_state(&app, PetState::Offline);
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        eprintln!("[kimi-pet] 已连接 kimi web（端口 {port}）");

        // 订阅当前所有会话的事件
        let client = reqwest::Client::new();
        let ids = list_session_ids(&client, port, &token).await;
        if ids.is_empty() {
            eprintln!("[kimi-pet] 会话列表为空，仅接收全局事件");
        } else {
            let frame = json!({
                "type": "subscribe",
                "id": "sub-0",
                "payload": { "session_ids": ids }
            });
            if let Err(e) = ws.send(Message::text(frame.to_string())).await {
                eprintln!("[kimi-pet] 订阅帧发送失败：{e}");
            }
        }

        let mut sessions: HashMap<String, SessionState> = HashMap::new();
        set_state(&app, PetState::Idle);

        while let Some(msg) = ws.next().await {
            let Ok(Message::Text(text)) = msg else {
                if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
                    break;
                }
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(event_type) = event.get("type").and_then(|v| v.as_str()) else {
                continue;
            };

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
                        if let Err(e) = ws.send(Message::text(frame.to_string())).await {
                            eprintln!("[kimi-pet] 补订阅 {id} 失败：{e}");
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
            if is_turn_over(event_type) {
                sessions.remove(session_id);
            } else if let Some(state) = map_event(event_type) {
                sessions.insert(session_id.to_string(), state);
            } else {
                continue;
            }
            set_state(&app, aggregate(&sessions));
        }

        eprintln!("[kimi-pet] WS 连接断开，{RETRY_DELAY:?} 后重连");
        set_state(&app, PetState::Offline);
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

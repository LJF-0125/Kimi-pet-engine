# kimi-code 协议变更影响分析与 v3 兼容方案

> 调研日期：2026-09-10
> 结论：**kimi-code 下个版本（0.42.0 之后）会删掉 v1 协议的 `streaming` 阶段，kimi-pet 的「编辑中」状态将失效；应迁移到同批引入的 `/api/v3/ws` 协议。**

## 1. 背景：kimi-code 侧发生了什么

kimi-code 仓库（MoonshotAI/kimi-code）在 0.42.0 发版（2026-09-09，`ci: release packages #3574`）之后合入了一批提交，其中两个与本项目直接相关：

| 提交 | PR | 内容 |
|---|---|---|
| `56ed9549` | #3678 | refactor(agent-core-v2): 移除 AgentActivityView，v1 agent phase 改在 kap-server 边缘投影 |
| `64505e36` | #3532 | feat(kap-server): 新增扁平实体消息协议（v3 WS + history API） |

**关键变更**：#3678 从 `packages/kap-server/src/protocol/events-zod.ts` 的 `agentPhaseSchema` 中**整段删除了 `streaming` 枚举**。当前 main 分支的 phase kind 只剩：

```
idle / running / tool_call / retrying / awaiting_approval / interrupted / ended
```

而 kimi-pet 的「编辑中」（answering）状态完全依赖 `phase.kind == "streaming" && stream != "thinking"` 判定（见 `src-tauri/src/kimi.rs` 的 `map_phase`）。streaming 删除后，agent 流式输出正文时 phase 只会是 `running`。

## 2. 影响评估

### 不受影响的部分（底层连接契约保持兼容）

- `~/.kimi-code/server.token` 读取、`~/.kimi-code/server/instances/*.json` 端口清单：未变
- REST `GET /api/v1/healthz`、`GET /api/v1/sessions`（`busy` / `pending_interaction` 字段）：未动
- WS `/api/v1/ws` 端点、`subscribe` 帧、应用层 ping/pong：#3532 的 v3 是**另开端点**，`transport/ws/v1/` 目录零改动
- kimi-pet 消费的事件名全部还在广播（已对照 main 分支 `sessionEventBroadcaster.ts` 确认）：
  `agent.status.updated`、`event.approval.requested/resolved`、`event.question.requested/answered/dismissed`、`event.session.created`、`turn.ended`、`turn.step.interrupted`

### 实质性回归

- **`streaming` 阶段从协议中删除** → 「编辑中」状态再也不会触发，agent 输出正文时桌宠只会显示「思考中」，`answering.gif` 失效。

### 次要噪点

- 新增的 `ended` / `awaiting_approval` / `interrupted` 三种 kind，`map_phase` 不认识，会打「未知 phase kind」日志并按空闲处理：
  - `ended` / `interrupted`：语义恰好正确（转为空闲）
  - `awaiting_approval`：会闪一下空闲再被 `event.approval.requested` 拉回待审核，基本无碍

### 时间窗口

- 这批提交**尚未发布**，最新 release 0.42.0 不受影响。
- **升级到 0.42.0 之后的版本时「编辑中」必失效** —— 在那之前完成迁移即可。

## 3. 兼容方案

### 方案 A（推荐）：v3 优先、v1 兜底的双栈自适应

连接发现层（token、实例清单、healthz 扫描）不动，找到端口后探测协议：

```
连 ws://127.0.0.1:{port}/api/v3/ws（同样的 kimi-code.bearer.{token} 子协议头）
  ├─ 升级成功 → 新版 kimi-code，走 v3 状态映射
  └─ 升级失败(404) → 旧版(≤0.42.0)，走现有 v1 代码路径（streaming 逻辑原样保留）
```

#### v3 协议要点（来自 #3532 实际代码）

- **握手**：服务端先发 `hello` → 客户端回 `subscribe {id, session_id, agent_ids?, omit?}` → 服务端回 `ack`（按 id 匹配）→ 推**恢复载荷**（在途实体 + 待处理交互 + 运行中任务 + `session.state`）→ 进入实时流
- **心跳**：WebSocket 协议层 ping/pong，tokio-tungstenite 自动应答（v1 的应用层 pong 不再需要）
- **每帧都带** `session_id` / `agent_id` / `timestamp`
- **关键消息类型**：
  - `assistant.delta` `{message_id, text}` —— 正文流式输出 → **编辑中**（streaming 的完美替代）
  - `thinking.delta` —— 推理流 → 思考中
  - `interaction` `{interaction_id, kind: approval|question, status: pending|approved|rejected|cancelled|answered|dismissed}` → 待审核 / 解除
  - `turn` / `session.state` → 轮次边界、忙闲
- **订阅即得全量**：恢复载荷让初始化不再依赖 REST 轮询校准（轮询仍保留作兜底，API 未变）

#### 多会话：单连接 + 逐会话订阅帧

已核实 v3 服务端实现（`wsConnectionV3.ts` / `wsV3Hub.ts`）：**一条连接支持多路订阅**（内部是 `subscriptions: Map<session_id, subscriber>`），不需要一会话一连接。官方 kimi-inspect 客户端的一会话一连接只是用法，不是协议限制。

架构可完全沿用现状：

```
发现服务 → 连 /api/v3/ws（单连接）
  ├─ 初始：REST 拉会话列表 → 每个会话发一个 subscribe {id, session_id}
  ├─ 3 秒 REST 轮询（现有逻辑保留）：发现新会话 → 同一条连接补发 subscribe
  └─ 会话删除/归档 → 发 unsubscribe {session_id}，移出忙态表
```

- 状态聚合零改动：现有 `HashMap<String, SessionState>` + `aggregate()` 原样复用
- **`omit` 可裁流量**：subscribe 支持 `omit: [...]` 排除不关心的消息类型（如 `tool.progress`）
- **重复 subscribe 幂等**：同一会话重发 subscribe 是替换而非报错，断线重连后把已知会话全部重订一遍即可
- delta 帧只在会话活跃时产生，订阅大量历史会话的闲时开销可忽略

#### v3 状态映射表

| v3 消息 | 桌宠状态 |
|---|---|
| `thinking.delta` | 思考中 |
| `assistant.delta` | 编辑中 |
| `interaction`（pending） | 待审核 |
| `interaction`（approved/answered/dismissed 等） | 回到思考中 |
| `turn` 结束 / 无在途实体 | 空闲 |

注意：delta 停止到 step 结束之间有间隙，需加约 1.5~2 秒尾随防抖再回落「思考中」。

### 方案 B（不推荐）：留在 v1 用 `subscribe_v2` transcript 档位

v1 的 `subscribe_v2` 可按档位订阅 `transcript.ops` 增量帧，从 assistant 消息 append 帧推「编辑中」。但 ops 是 L2 reducer 实体模型（upsert/append/remove），解析负担重，且 `subscribe_v2` 在 0.42.0 是否存在未验证——若没有照样要写双栈。投入产出比不如直接上 v3。

### 方案 C（保底一行改）：优雅降级

删掉 `map_phase` 的 answering 分支，忙即「思考中」。代价是 `answering.gif` 永久退休。适合作为「新版 kimi-code 已发布但方案 A 未写完」时的过渡补丁。

## 4. 迁移的净收益与代价

**变简单**：
- 状态映射从「枚举推断」变成「直接信号」，`map_phase` 的 switch 退化为查表
- 心跳不用手写（协议层处理）
- 订阅即得全量状态，REST 轮询降级为纯兜底

**变复杂（仅过渡期）**：
- 双栈期间同时背 v1/v3 两条代码路径，停止兼容 ≤0.42.0 后可删 v1
- delta 防抖逻辑（新增，约一个定时器）

**长期收益**：v3 是官方 web 应用（kimi-inspect）自用的协议，维护优先级高于 v1 legacy 层；一次迁移提前消化后续兼容性问题。

## 5. 落地清单

- [ ] 协议探测：先 `/api/v3/ws` 升级，失败回退 v1 现有路径
- [ ] v3 连接任务：hello → 逐会话 subscribe → ack → 恢复载荷初始化
- [ ] delta 防抖：1.5~2s 无 delta 且仍忙 → 回落思考中
- [ ] 状态映射按第 3 节表实现，未知 type 一律忽略（`serde_json::Value` 宽松解析，v3 字段可能继续生长）
- [ ] 保留 REST 3 秒轮询：新会话发现 + 忙态最终权威
- [ ] subscribe 带 `omit` 裁掉不需要的消息类型
- [ ] 断线重连：重连后全量重订（幂等）
- [ ] 双栈稳定运行一段时间后，视情况删除 v1 路径

## 6. 可选：给官方提 issue

建议官方在 v1 legacy 投影里保留 streaming 语义（他们删它是因为内部不再区分流式阶段，但 v1 是兼容层，可补回）。成本一句话，不阻塞自身迁移。

## 参考

- kimi-code 仓库：<https://github.com/MoonshotAI/kimi-code>
- #3678（删 streaming）：commit `56ed9549`，涉及 `packages/kap-server/src/protocol/events-zod.ts`、`services/legacyStatus/`
- #3532（v3 协议）：commit `64505e36`，涉及 `packages/kap-server/src/protocol/messages/`、`transport/ws/v3/`；客户端参考实现 `apps/kimi-inspect/src/transcript/ws.ts`
- kimi-pet 现状代码：`src-tauri/src/kimi.rs`（`map_phase`、WS 主循环 `run`、REST 校准 `reconcile`）

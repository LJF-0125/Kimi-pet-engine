# kimi-code 协议变更影响分析与兼容方案（已落地）

> 调研日期：2026-09-10；**实测验证与修复：2026-09-19**
> 结论：kimi-code 在 0.42.0 之后删掉了 v1 协议的 `streaming` 阶段，「编辑中」失效（桌宠一直显示「思考中」）。
> **修复已落地**：不迁 v3，而是在 v1 WS 上叠加 `subscribe_v2`（transcript 增量帧）识别正文流，旧版服务自动回退原逻辑。

## 0. 2026-09-19 实测验证（对本机运行中的服务）

实测环境：**桌面版 Kimi Code 2.0.1**（`Kimi Code.exe` 内嵌 kap-server，CLI 0.43.1 同款代码线）。

| 验证项 | 方法 | 结果 |
|---|---|---|
| `streaming` 已删 | 解包 app.asar 读 `agentPhaseSchema` | phase kind 只剩 `idle / running / tool_call / retrying / awaiting_approval / interrupted / ended` |
| 「一直思考中」复现 | 用户实测 | 流式输出时 phase 只报 `running` |
| `/api/v3/ws` | curl 升级探测 | **404，桌面版不存在该路由**（CLI 二进制里有 v3 字符串，但桌面版未发布）→ 原方案 A 前提不成立 |
| `subscribe_v2` | 实测连 v1 WS 订阅 delta 档并发 prompt | 可用：ack 接受，`transcript.reset` + 实时 `transcript.ops` 正常推送 |
| v1 原有事件 | asar 静态检查 | `agent.status.updated`、`event.approval.requested/resolved`、`event.question.*`、`turn.ended` 等全部还在 |

实测抓到的 v2 消息形态（一次完整轮次约 116 帧 ops）：

- `frame.upsert`：`frame.kind` ∈ `text`（带 `role:"assistant"|"user"`）/ `thinking` / `tool_call` / `notice`，帧 id 为 `frameId`
- `append`：`target:{type:"frame", frameId, ...}` + `offset` + `text`（不带帧类型，需查帧表回查）
- `step.upsert`：state ∈ `running | completed | interrupted | failed`
- `turn.upsert`：state ∈ `queued | running | completed | failed | cancelled`
- `interaction.upsert`：`status:"pending"` 即待审核
- `event.session.work_changed`：`busy` / `pending_interaction`，现成的忙闲信号
- 订阅帧：`{"type":"subscribe_v2","id":"...","payload":{"session_id":"...","transcript":{"*":"delta"}}}`；
  档位 `off / turn / block / delta`，delta 档才推 `append`
- ack：`code==0` 成功；旧版服务不认识 `subscribe_v2` 会回错误 ack，忽略即可

## 1. 背景：kimi-code 侧发生了什么

kimi-code 仓库（MoonshotAI/kimi-code）在 0.42.0 发版（2026-09-09）之后合入：

| 提交 | PR | 内容 |
|---|---|---|
| `56ed9549` | #3678 | refactor(agent-core-v2): 移除 AgentActivityView，v1 agent phase 改在 kap-server 边缘投影（**整段删除 `streaming` 枚举**） |
| `64505e36` | #3532 | feat(kap-server): 新增扁平实体消息协议（v3 WS + history API） |

## 2. 影响评估（已实测确认）

### 不受影响

- `~/.kimi-code/server.token`、`server/instances/*.json` 端口清单：未变
- REST `GET /api/v1/healthz`、`GET /api/v1/sessions`（`busy` / `pending_interaction`）：未动
- WS `/api/v1/ws` 端点、v1 `subscribe`、应用层 ping/pong：未动
- kimi-pet 消费的 v1 事件名全部还在广播

### 实质性回归（已发生）

- **`streaming` 阶段删除** → 「编辑中」不再触发，agent 输出正文时桌宠一直显示「思考中」。

## 3. 最终方案（已实现）：v1 双栈 —— v1 事件 + subscribe_v2 transcript 增量

原方案 A（探测 `/api/v3/ws`）前提被证伪：桌面版 2.0.1 没有 v3 路由。原方案 B 实测可行且覆盖面更广
（桌面版与 CLI 都有 subscribe_v2），遂采用：

```
连 /api/v1/ws（同一条连接）
  ├─ v1 subscribe（所有会话）：agent.status.updated / 审批事件 / turn.ended —— 保留
  ├─ subscribe_v2（每个会话一帧，transcript {"*":"delta"}）：transcript.ops 增量
  │    ├─ frame.upsert(text, role=assistant) / append 到正文帧 → 编辑中
  │    ├─ frame.upsert(thinking|tool_call) / step.upsert → 思考中
  │    ├─ interaction.upsert(pending) → 待审核
  │    └─ turn.upsert 终态 / work_changed(busy=false) → 空闲
  └─ 旧版服务回错误 ack → 忽略，「编辑中」继续靠 streaming phase（原逻辑保留）
```

实现要点（`src-tauri/src/kimi.rs`）：

- 守卫 A：会话处于待审核时，跳过一切非待审核迁移
- 守卫 B：编辑中防抖——正文 delta 停更超过 2 秒（`ANSWER_TAIL`）才回落思考中
- 守卫 C：step/append/interaction 的非忙迁移只作用于已在忙态表的会话，防重连回放误标忙
- REST 3 秒轮询保留：新会话发现（补发 subscribe + subscribe_v2）+ 忙态最终权威

### 历史方案记录

- 方案 A（v3 WS 双栈自适应）：✗ 桌面版无 `/api/v3/ws`，不可行
- 方案 B（subscribe_v2 transcript）：✓ 已采用
- 方案 C（删 answering 优雅降级）：未采用（B 已验证可行）

## 4. 参考

- kimi-code 仓库：<https://github.com/MoonshotAI/kimi-code>
- #3678（删 streaming）：commit `56ed9549`；#3532（v3 协议）：commit `64505e36`
- kimi-pet 实现：`src-tauri/src/kimi.rs`（`sv2_frame`、`transcript.ops` 分支、`to_thinking` 守卫、`ANSWER_TAIL` 防抖）
- 实测脚本：`ws_probe_sv2.py`（subscribe_v2 抓取工具）

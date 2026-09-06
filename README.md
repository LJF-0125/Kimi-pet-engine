# kimi-pet

Kimi Code 桌宠：一个常驻桌面的透明小窗，跟着本机 `kimi web` 服务的实时事件切换 GIF。

## 四种状态

| 状态 | 触发时机 |
| --- | --- |
| 思考中 | 轮次开始 / 模型推理 / 调用工具干活 |
| 回答中 | 正在流式输出回复正文 |
| 待审核 | 弹出权限审批或提问，等你处理 |
| 空闲中 | 一轮结束 / 服务刚连上；服务离线时 GIF 会变灰 |

## 交互

- **拖动**：按住桌宠拖到任意位置
- **双击**：用系统浏览器打开 Kimi Code Web 界面
- **右键**：打开设置窗口，为四种状态各选一张 GIF（透明底最佳），即时生效、自动保存

## 原理

`kimi web` 启动的本地服务暴露 REST + WebSocket API（默认 `127.0.0.1:58627`）。
本应用自动读取 `~/.kimi-code/server.token`、探测端口、连接 `/api/v1/ws` 事件流，
把 `turn.started` / `assistant.delta` / `event.approval.requested` / `turn.ended` 等事件
映射为四种状态推送给窗口。服务断开自动重连。

注意：该 API 是实验性的，字段可能随 Kimi Code 版本变化。

## 运行（需要 Rust 环境）

```sh
# 1. 安装 Rust（Mac / Linux）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Windows 下载 rustup-init.exe：https://rustup.rs

# 2. 安装 Tauri CLI
cargo install tauri-cli --version "^2"

# 3. 开发模式运行（需先在本机跑着 kimi web）
cd kimi-pet
cargo tauri dev
```

## 打包

项目自带一个占位图标（`src-tauri/icons/icon.png`，蓝色笑脸），想换正式的：

```sh
cargo tauri icon path/to/icon.png   # 生成全套尺寸图标
cargo tauri build
```

产物在 `src-tauri/target/release/bundle/`。也可以把项目推到 GitHub，
用 GitHub Actions 的 `tauri-apps/tauri-action` 同时出 macOS 和 Windows 安装包。

## 使用前提

桌宠和 `kimi web` 必须在**同一台电脑**上运行（它连的是 127.0.0.1 的本地服务）。
先用 `kimi web` 启动 Kimi Code 的 web 模式，再启动桌宠。

# 自动更新签名密钥：生成与使用指南

> 面向维护者（包括 AI 助手）：kimi-pet 的应用内自动更新（`tauri-plugin-updater`）依赖一对
> 签名密钥。本文说明密钥的作用、生成方法、配置位置和安全注意事项。
>
> **当前状态**：代码已就绪（`src-tauri/src/updater.rs`），但
> `src-tauri/tauri.conf.json` 里的 `plugins.updater.pubkey` 仍是占位符
> `PASTE_PUBLIC_KEY_HERE`，密钥尚未生成。完成下文第 1～3 步后自动更新才会真正生效。

## 密钥是干什么的

自动更新 = 应用从 GitHub Releases 下载新安装包并替换自己。为防止下载链路被篡改后
装上恶意版本，更新器强制验签：

- **私钥**：CI 打包时给更新包签名（生成 `.sig` 文件）
- **公钥**：编译进客户端 exe，用户端下载后用它验签，对不上就拒绝安装

没有这对密钥，tauri-action 不会产出更新包，客户端的更新检查也会失败。

## 第 1 步：生成密钥对

在有 Tauri CLI 的开发机上执行（只需一次）：

```sh
# 没有 CLI 的话先装：cargo install tauri-cli --version "^2"
# 或者用 npm：npx @tauri-apps/cli signer generate -w ~/.tauri/kimi-pet.key
cargo tauri signer generate -w ~/.tauri/kimi-pet.key
```

- 会提示设置密码（可直接回车留空，CI 配置会简单一点；设了就要多配一个 secret）
- 生成两个文件：
  - `~/.tauri/kimi-pet.key` —— **私钥，绝不可提交进仓库**
  - `~/.tauri/kimi-pet.key.pub` —— 公钥，可以公开

## 第 2 步：公钥填进 tauri.conf.json

打开 `~/.tauri/kimi-pet.key.pub`，把**文件全部内容**（含 `untrusted comment:` 那行
和 base64 那行）原样填入 `src-tauri/tauri.conf.json`：

```json
"plugins": {
  "updater": {
    "pubkey": "<这里换成 .pub 文件的完整内容>",
    ...
  }
}
```

替换掉占位符 `PASTE_PUBLIC_KEY_HERE` 后提交。

## 第 3 步：私钥配进 GitHub Secrets

仓库页面 → **Settings → Secrets and variables → Actions → New repository secret**：

| Secret 名称 | 内容 |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | `~/.tauri/kimi-pet.key` 文件的**全部内容** |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 生成时设的密码；留空则无需此项 |

也可以用 `gh` CLI（在能访问私钥文件的机器上）：

```sh
gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/kimi-pet.key
```

## 原理：之后会发生什么

配置完成后一切自动，无需再动：

1. 推 `v*` tag → `.github/workflows/release.yml` 触发 tauri-action
2. tauri-action 检测到 `plugins.updater` 配置且 secrets 存在 → 自动构建更新包
   （Windows 是 NSIS 安装包，macOS 是 `.app.tar.gz`）并用私钥签名
   （前提：`tauri.conf.json` 的 `bundle.createUpdaterArtifacts` 已设为 `true`，
   否则只出普通安装包、不生成 `.sig`/`latest.json`）
3. 发布 release 时附带签名后的更新包和 `latest.json`（含各平台版本号、下载地址、签名）
4. 客户端启动时及每 6 小时请求
   `https://github.com/LJF-0125/Kimi-pet-engine/releases/latest/download/latest.json`，
   比对版本号；有新版本弹窗询问，用户确认后下载、用内置公钥验签、安装并重启

注意：`latest.json` 只在 release **正式发布**后可访问，draft 阶段客户端拉不到——
现在的 draft 流程不受影响，手动发布后即生效。

## 安全注意事项

- **私钥丢失** = 已发布的客户端永远无法验签通过新版本（公钥焊死在旧 exe 里），
  只能让用户手动下载重装。生成后立刻把 `kimi-pet.key` 备份到密码管理器/离线存储
- **私钥泄露** = 他人可伪造你的"官方更新"。立即重新生成密钥对、更新 Secrets、
  换新公钥发版，并通知所有用户升级
- 不要把私钥写进仓库、文档或聊天记录；`.gitignore` 管不到 `~/.tauri`，
  但也别把它复制进项目目录

## 验证清单（改完密钥配置后）

1. `cd src-tauri && cargo check` —— 确认编译通过、Cargo.lock 更新后提交
2. 发一个 tag（如 `v0.1.5`）→ CI 产物里应出现 `.sig` 文件和 `latest.json`
3. 安装该版本，再发一个更高版本的 tag（如 `v0.1.6`）
4. 打开旧版本：应在启动后 1 分钟内弹出「发现新版本」对话框，
   点「立即更新」后自动下载、安装、重启成新版本

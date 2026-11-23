# OpenMirasim 技术栈与构建环境规范

本文档明确 OpenMirasim 采纳 [`design-review.md`](design-review.md)（R1–R7）决议后的技术选型、选型依据与构建环境要求。

---

## 1. 技术栈矩阵

### 1.1 Linux 中心服务端（`om-server` + `om-core`）

| 类别 | 选型 | 说明 |
| --- | --- | --- |
| 语言与工具链 | **Rust（Stable，MSRV 1.85）** | 低内存占用、单二进制部署、可靠承载 30 人团队多路 Agent 会话 |
| 异步运行时 | **Tokio** | 高并发网络 I/O、单会话 Actor 调度、子进程管理与超时回收 |
| HTTP / WebSocket | **Axum + Tower** | REST 控制面、`/ws/events`（JSON 文本帧）、`/ws/pty/:id`（Binary 帧）及静态前端托管 |
| 跨端类型契约 | **`serde` + `ts-rs`** | 以 `om-core` 的 Rust 类型为单一事实源，自动生成 TypeScript 类型并在 CI 校验 |
| 数据存储 | **`rusqlite`（SQLite WAL + 单写线程）** | 零外部数据库运维、编译快、原生支持 FTS5 与精细 PRAGMA 控制；事件直接存 `events(session_id, seq, ...)` |
| PTY 与录制 | **`portable-pty` + asciicast v2（`.cast`）** | Phase 3 原生终端视图与兜底；原始字节写环形缓冲与 `.cast` 文件，不进 SQLite |
| Linux 裸金属沙箱 | **`bubblewrap`（`bwrap`）+ `systemd-run`/cgroups v2** | 毫秒级无守护进程隔离（Worktree bind-mount、CPU/内存/PIDs/网络隔离） |
| Agent 接入协议 | **ACP（JSON-RPC 2.0 stdio）** > `stream-json` > PTY | 优先统一对接 `opencode`、`dsh`、`cc`、`codexcli`；Antigravity GUI 通过独立外置 `om-gui-bridge` 接入 |
| Git 管理 | **`git` CLI（Worktree 隔离）** | 稳定可控的 Worktree 创建/清理/状态/Diff |
| 可观测性 | **`tracing` + `tracing-subscriber`** | 按 `space_id` / `session_id` 结构化日志追踪 |

### 1.2 CLI 与 TUI（`om-cli`）

| 类别 | 选型 | 说明 |
| --- | --- | --- |
| 命令行与控制面 | **Clap（derive）+ Serde JSON** | `openmirasim serve`、脚本化 `openmirasim ctl`（纯 JSON 输出），直接复用 `om-core` 类型 |
| 终端工作台（Phase 3） | **Ratatui + Crossterm** | 无头 Linux 服务器与终端用户的多会话 TUI |

### 1.3 Web 工作台、PWA 与桌面端（`apps/web` + Phase 3 `apps/tauri`）

| 类别 | 选型 | 说明 |
| --- | --- | --- |
| 前端框架 | **React 19 + TypeScript 5 + Vite** | 一套代码服务桌面浏览器、移动端 PWA 与 Tauri 桌面壳 |
| 包管理 | **pnpm Workspace**（固定 `engines` & `packageManager`） | 统一前端依赖与类型生成脚本 |
| 移动端方案（Phase 2） | **PWA + Service Worker + Web Push** | 手机端查看进度与实时审批，省去早期 Android NDK / Xcode 维护成本（iOS 需“添加到主屏幕”开启推送） |
| 桌面端薄壳（Phase 3） | **Tauri v2** | 托盘、原生通知、OS Keychain、多服务器切换；不承载核心业务状态 |
| 终端与代码视图 | **xterm.js**（Phase 3）+ Diff 查看器（Phase 1） | 渲染远程 PTY 流与 Worktree Git Diff |

### 1.4 质量与测试

| 类别 | 选型 | 说明 |
| --- | --- | --- |
| Rust 检查 | `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` | 每次 PR 必跑 CI 门禁 |
| 前端检查 | `pnpm typecheck`（`tsc --noEmit`）+ 单元测试 | 类型安全与生成契约一致性检查 |
| 端到端测试 | **Playwright**（Mock Harness 驱动） | 验证流式对话、`last_seq` 断线续传、重启恢复、审批流与沙箱越界拦截 |

---

## 2. 延后引入的技术（明确不在 Phase 0–2）

- **Protobuf / gRPC / Buf**：Phase 0–2 全链路使用 Axum REST + WebSocket + `ts-rs`，直到需要对外发布多语言 SDK 时再按需引入。
- **Tauri Mobile（Android / iOS 原生构建）**：Phase 0–3 由 PWA + Web Push 承担移动端体验，Phase 4 再按需评估原生壳。
- **Windows 服务端支持**：服务端先专注 Linux 裸金属（客户端 Web / Tauri 桌面仍跨平台支持 Windows / macOS / Linux）。

---

## 3. 构建环境与工具链要求

### 3.1 基础工具链版本

- **Rust**：通过根目录 [`rust-toolchain.toml`](../rust-toolchain.toml) 固定 `stable` 通道与 `rustfmt`/`clippy` 组件（MSRV `1.85`）。
- **Node.js / pnpm**：通过根目录 [`package.json`](../package.json) 的 `engines`（`node >= 22`、`pnpm >= 10`）与 `packageManager` 字段约束。
- **Linux x86_64 链接加速**：通过仓库级 [`.cargo/config.toml`](../.cargo/config.toml) 配置 `clang` + `lld`（不再修改开发者本机全局 `$CARGO_HOME/config.toml`）。

### 3.2 Linux（Ubuntu 24.04）一键初始化脚本

仓库提供幂等、非交互的 [`scripts/setup-env.sh`](../scripts/setup-env.sh)，按阶段按需安装依赖：

```bash
# 默认：Phase 0–2 服务端 + Web 最小依赖（build-essential、clang、lld、pkg-config、libssl-dev、libsqlite3-dev、sqlite3、bubblewrap）
bash scripts/setup-env.sh

# 云端开发机（.agents/environment.json 自动调用，额外完成非交互 shell PATH 适配）
bash scripts/setup-env.sh --cloud

# Phase 3 开发 Tauri v2 Linux 桌面壳时追加桌面依赖（WebKitGTK 4.1 / GTK 3 / AppIndicator / librsvg / xvfb / cargo-tauri）
bash scripts/setup-env.sh --desktop

# Phase 4 按需构建 Android 时追加 Android NDK 与 Rust Android targets
bash scripts/setup-env.sh --android
```

- 默认执行**不会修改本机 `~/.bashrc`**，也**不会写入全局 `$CARGO_HOME/config.toml`**。
- 云端环境配置文件 [`.agents/environment.json`](../.agents/environment.json) 默认调用 `bash scripts/setup-env.sh --cloud`，确保新开云端 VM 在数秒内完成服务端与 Web 构建环境初始化。

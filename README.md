# OpenMirasim

OpenMirasim 是一个开源的、以**单台 Linux 裸金属中心服务器**为核心的**团队协作式 C/S 架构跨平台 AI Agent 工作台**（面向 **30 人以内团队**，架构与交互理念借鉴 [Mirasim](https://mirasim.ai/)）。

它将多 Agent 并发会话、ACP 结构化交互与终端 PTY、Git Worktree 隔离、Linux 沙箱执行、工具权限审批与团队实时协作收敛到单台自托管 Linux 服务器，并通过统一协议为 Web 工作台、移动端 PWA、Tauri 桌面薄壳与 CLI/TUI 提供零上下文丢失的多端体验。

## 核心特性规划

- **单台 Linux 裸金属中心化架构**：所有会话状态、工作区、Git Worktree、Agent 进程与事件流由 `om-server` 统一托管，支持按 `last_seq` 断线续传与重启恢复。
- **团队多人协作（<=30 人）**：围绕团队空间（Space）提供 RBAC 权限管理、多成员实时同步、Queue & Steer 与原子化审批流；默认采用 BYOK，并通过宿主机鉴权代理隔离上游密钥。
- **结构化优先的多 Agent 接入**：
  - 优先通过 **ACP（Agent Client Protocol）** / `stream-json` 接入 **`opencode`**、**`dsh`**（DeepSeek Harness）、**`cc`**（Claude Code）、**`codexcli`**（Codex CLI）；
  - PTY 作为原生终端视图与兜底（asciicast v2 `.cast` 录制）；
  - **Antigravity GUI** 通过独立外置桥接进程（`om-gui-bridge` + Linux `Xvfb` 每用户独立隔离 + 可选 noVNC）接入，不污染核心服务端。
- **渐进式跨平台形态**：Web 工作台优先 -> 移动端 **PWA + Web Push** 审批 -> **Tauri v2 桌面薄壳**与 **Ratatui TUI**。

## 技术栈概览

| 层级 | 技术选型 | 说明 |
| --- | --- | --- |
| 中心服务端（Linux） | **Rust**（Axum + Tokio） | 单二进制部署、低内存占用、高并发 Actor 会话调度 |
| 协议与类型契约 | **Axum REST + WebSocket** + **`ts-rs`** | Rust `om-core` 为单一事实源生成 TS 类型；`/ws/events` 与 `/ws/pty` 分离 |
| 数据存储 | **`rusqlite`（SQLite WAL + 单写线程）** + `.cast` | 元数据与结构化事件入 SQLite；PTY 原始帧写环形缓冲与 asciicast v2 文件 |
| Linux 执行沙箱 | **`bubblewrap`（`bwrap`）+ `systemd-run`/cgroups v2** | 裸金属原生隔离：Worktree 挂载边界与 CPU/内存/PIDs 限制 |
| Web 与移动端 | **React 19 + TypeScript + Vite + pnpm**（含 **PWA**） | 一套代码服务浏览器工作台、移动端推送审批与桌面渲染层 |
| 桌面壳与终端 | **Tauri v2**（薄壳）+ **Clap / Ratatui**（`om-cli`） | 托盘/通知/Keychain、脚本化 `ctl` 控制面与终端 TUI |

## 文档导航

- [系统架构设计](docs/architecture.md)：C/S 拓扑、核心领域模型、Harness 抽象、Linux 裸金属沙箱与 Monorepo 结构。
- [技术栈与构建环境说明](docs/tech-stack.md)：详细选型依据、工具链版本约束与一键初始化说明。
- [路线图与协作分工](docs/roadmap.md)：Phase 0–4 可复现 Demo 验收清单、hark（Opus 5.5）与 Antigravity 的模块分工及并行防冲突规范。
- [架构与技术栈评审决议](docs/design-review.md)：R1–R7 评审意见与最终采纳结论。

## 快速初始化（Ubuntu 24.04）

```bash
# 安装 Phase 0–2 最小系统构建依赖（含 clang / lld / sqlite3 / bubblewrap）
bash scripts/setup-env.sh

# 如需开发 Phase 3 Tauri 桌面壳或 Phase 4 Android 构建，可按需追加：
# bash scripts/setup-env.sh --desktop
# bash scripts/setup-env.sh --android
```

## 许可证

本项目采用 [Apache License 2.0](LICENSE) 开源许可。

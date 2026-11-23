# OpenMirasim

OpenMirasim 是一个开源的、以**单台中心服务器**为核心的**团队协作式 C/S 架构跨平台 AI Agent 工作台**（架构与交互理念借鉴 [Mirasim](https://mirasim.ai/)）。

它将多 Agent 并发会话、终端 PTY、Git Worktree 隔离、工具权限审批、MCP/Skills 扩展与团队实时协作收敛到一台可自托管的中心服务端，并通过统一的协议层为桌面端、Web 端、移动端以及 CLI/TUI 提供零上下文丢失的多端体验。

## 核心特性规划

- **单服务器中心化架构**：所有会话状态、工作区、Git Worktree、Agent 进程与审计日志由单台 `openmirasim-server` 统一托管，支持断线重连与多端实时同步。
- **团队多人协作（Team Spaces）**：围绕团队空间（Space）组织成员、工作区与会话；支持多人同时查看、接力、排队插队（Queue & Steer）及审批 Agent 操作。
- **多 Agent 并发与编排**：统一调度多种 CLI/API Agent（如 Claude Code、Codex、OpenCode 及自定义 Harness），支持并行会话、会话分支、会话间消息协作与定时任务。
- **跨平台统一体验**：
  - **桌面端**：基于 Tauri v2 + React/TypeScript（macOS / Windows / Linux），支持多窗格工作台、聊天视图与原生终端 PTY 视图无缝切换。
  - **Web 工作台**：服务端直接托管同一套前端 SPA，浏览器零安装接入。
  - **移动端**：基于 Tauri v2 Mobile（Android / iOS），随时在手机端查看进度、审批工具调用与继续对话。
  - **CLI / TUI**：纯 Rust 命令行与终端界面，支持无头服务器部署与脚本化控制（类 `ui-cli`）。

## 技术栈概览

| 层级 | 技术选型 | 说明 |
| --- | --- | --- |
| 中心服务端 | **Rust**（Axum + Tokio + Tonic） | 单二进制部署、低内存占用、高并发异步 I/O 与 PTY 管控 |
| 数据存储 | **SQLite（WAL 模式）** + 事件日志 | 面向单机中心服务的零运维持久化与完整会话回放 |
| 协议与接口 | **Protobuf v3 + Buf**（gRPC + HTTP/WebSocket） | 跨语言单一事实源，强类型驱动 Rust / TypeScript 代码生成 |
| 跨平台客户端壳 | **Tauri v2**（Desktop + Android/iOS） | 系统原生 WebView、体积小、与服务端共享 Rust 核心协议 Crate |
| 共享前端 UI | **React + TypeScript + Vite + pnpm** | 一套代码同时服务 Web 工作台、桌面端与移动端 |
| CLI / TUI | **Rust**（Clap + Ratatui） | 服务端运维、终端直接交互与自动化脚本驱动 |

## 文档导航

- [系统架构设计](docs/architecture.md)：C/S 拓扑、核心域模型、服务端模块、协议设计、安全模型与目录规划。
- [技术栈与构建环境说明](docs/tech-stack.md)：详细选型依据、依赖矩阵、构建工具链及各平台开发环境要求。
- [架构与技术栈评审意见（待讨论）](docs/design-review.md)：范围收敛、协议层、Agent 接入、沙箱安全、存储与构建脚本的改进提案。

## 许可证

本项目采用 [Apache License 2.0](LICENSE) 开源许可。

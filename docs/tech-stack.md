# OpenMirasim 技术栈与构建环境规范

本文档明确 OpenMirasim 各层技术选型、选型理由以及开发与 CI 构建环境依赖规范。

---

## 1. 技术栈矩阵

### 1.1 中心服务端与核心运行时（Rust）

| 类别 | 选型 | 用途与说明 |
| --- | --- | --- |
| 语言与工具链 | **Rust（Stable，Edition 2021/2024）** | 内存安全、单二进制部署、极低常驻内存，适合长期承载多路 Agent 与 PTY 会话 |
| 异步运行时 | **Tokio** | 高并发网络 I/O、Actor 会话调度、子进程与定时器管理 |
| HTTP / WebSocket | **Axum + Tower** | 现代类型安全路由、中间件鉴权、WebSocket 流式推送与静态前端资源托管 |
| gRPC 服务端 | **Tonic + Prost** | 面向原生客户端（Tauri Core / CLI）的强类型 RPC 与双向流 |
| 数据库 | **SQLite（WAL 模式）**（`sqlx` 或 `rusqlite`） | 面向单机中心服务的零运维持久化，支持 FTS5 全文检索与事务并发读 |
| 终端 PTY | **portable-pty**（或 `pty-process`） | 跨平台伪终端控制，承载 Claude Code / Codex / Shell 等交互式 CLI Agent |
| Git 与文件系统 | **`git` CLI + `git2` / `gix`** + `notify` | Worktree 生命周期管理、实时文件监听、状态与 Diff 计算 |
| 日志与可观测性 | **tracing + tracing-subscriber** | 结构化日志、按 `session_id` / `space_id` 追踪请求与 Agent 执行链路 |

### 1.2 CLI 与 TUI（Rust）

| 类别 | 选型 | 用途与说明 |
| --- | --- | --- |
| 命令行解析 | **Clap（derive）** | 统一 `openmirasim serve / tui / ctl / login` 子命令体系 |
| 终端界面 | **Ratatui + Crossterm** | 无头 Linux 服务器与终端重度用户的多会话 TUI 工作台 |
| 脚本化输出 | **Serde JSON** | 面向脚本与 Agent 的结构化 JSON 输出与声明式配置应用 |

### 1.3 跨平台客户端（Tauri v2 + React / TypeScript）

| 类别 | 选型 | 用途与说明 |
| --- | --- | --- |
| 桌面与移动端壳 | **Tauri v2**（Desktop + Mobile） | 复用系统原生 WebView，二进制体积小、内存开销低；Rust Core 可直接复用 `om-proto` 与 `om-core` |
| 前端框架 | **React 19 + TypeScript 5** | 组件生态成熟，适配复杂多窗格工作台、代码审阅与响应式移动布局 |
| 构建与包管理 | **Vite + pnpm Workspace** | 极速热更新、统一的 Monorepo 依赖管理与前后端共享类型包 |
| 样式与组件 | **Tailwind CSS + Radix UI / shadcn** | 一致的设计系统、高键盘可访问性与暗/亮色主题切换 |
| 终端与代码视图 | **xterm.js** + 轻量代码/Diff 查看器 | 在 Web 与 Tauri 窗格中原生渲染远程 PTY 输出与 Git Diff |
| 客户端状态与同步 | **Zustand / TanStack Query** + Event Stream Store | 维护 `last_seq` 增量事件同步、离线重连与乐观更新 |

### 1.4 协议与代码生成（Protobuf / gRPC）

| 类别 | 选型 | 用途与说明 |
| --- | --- | --- |
| IDL 与规范校验 | **Protobuf v3 + Buf** | 统一 API 契约、Lint 规范检查与 Breaking Change 防护 |
| 编译器与插件 | **protoc** + `tonic-build` / `@bufbuild/protobuf` | 从 `proto/` 自动生成 Rust 与 TypeScript 双端强类型代码 |

### 1.5 测试与质量保障

| 类别 | 选型 | 用途与说明 |
| --- | --- | --- |
| Rust 质量 | `cargo fmt`、`cargo clippy`、`cargo test` | 编译期类型约束、静态检查与服务端单元/集成测试 |
| 前端质量 | TypeScript `tsc --noEmit`、ESLint / Biome、Vitest | 类型安全、代码风格检查与前端逻辑单元测试 |
| 端到端测试 | **Playwright** + **Xvfb**（Linux 无头环境） | 验证 Web 工作台、服务端 API 与多客户端实时协作场景 |

---

## 2. 关键选型理由

1. **为什么中心服务端和 CLI 选 Rust？**
   - 单台服务器需要同时托管数十个 Agent 子进程、PTY 字节流、WebSocket 扇出与文件监听；Rust 的低内存占用与零 GC 停顿能把机器资源留给 AI Agent 与项目编译本身。
   - 服务端与 Tauri 客户端壳同属 Rust Workspace，领域类型、协议编解码与鉴权逻辑可直接跨端复用。
2. **为什么跨平台客户端选 Tauri v2 + 共享 React UI？**
   - 一套 React/TypeScript 前端即可覆盖 **Web 工作台（零安装）+ 桌面端（Windows/macOS/Linux）+ 移动端（Android/iOS）**，避免多端重复维护业务逻辑。
   - 相比 Electron，Tauri v2 省去自带 Chromium 的 150MB+ 内存与包体开销，与开发者本机运行多个编码 Agent 的场景更契合。
3. **为什么数据库默认选 SQLite（WAL）？**
   - 项目以单台自托管服务器为核心，SQLite（WAL 模式）零外部进程依赖、备份即复制单文件，读并发高且足以支撑团队级并发会话；后续如有超大团队需求，可通过 `om-storage` 抽象层平滑扩展至 PostgreSQL。
4. **为什么引入 Protobuf + Buf？**
   - 系统同时存在 Rust（服务端/CLI/Tauri Core）与 TypeScript（Web/桌面/移动 UI）两套语言，使用 Protobuf 作为单一事实源可避免手写 DTO 漂移，同时原生支持 gRPC 双向流。

---

## 3. 构建环境与工具链要求

### 3.1 基础工具链

| 工具 | 最低 / 推荐版本 | 用途 |
| --- | --- | --- |
| Rust (`rustup`, `cargo`, `clippy`, `rustfmt`) | Stable（>= 1.85） | 服务端、CLI/TUI 与 Tauri 原生层编译 |
| Node.js + pnpm | Node >= 22，pnpm >= 10 | 前端工作台、TS SDK 构建与 Monorepo 管理 |
| Tauri CLI (`cargo-tauri`) | `^2.0` | Desktop 与 Mobile（Android/iOS）构建打包 |
| Protobuf & Buf | `protoc` >= 3.21、`buf` >= 1.50 | Protobuf Lint、Breaking 检查与代码生成 |
| SQLite | `libsqlite3-dev` + `sqlite3` | 本地数据库编译与调试 |
| 快速链接器 | `lld`（LLVM Linker）+ `clang` | 显著加速 Rust / Tauri 增量链接速度 |

### 3.2 Linux（Ubuntu 24.04）系统构建依赖

构建 Tauri v2 桌面端、Rust 原生依赖与 Protobuf 需安装以下系统包：

```bash
sudo apt-get update && sudo apt-get install -y --no-install-recommends \
  libwebkit2gtk-4.1-dev \
  libgtk-3-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  libxdo-dev \
  patchelf \
  build-essential \
  pkg-config \
  libssl-dev \
  libsqlite3-dev \
  sqlite3 \
  lld \
  protobuf-compiler \
  libprotobuf-dev \
  xvfb \
  curl \
  wget \
  file
```

### 3.3 Android 移动端交叉编译环境（Linux / macOS / Windows）

- **JDK**：OpenJDK 17 或 21（配置 `JAVA_HOME`）。
- **Android SDK**：Platform（API 34+）、Build-Tools（34/35）、Platform-Tools、Command-line Tools（配置 `ANDROID_HOME`）。
- **Android NDK**：推荐使用 **NDK r28（`28.2.13676358`）**（默认支持 Android 16KB 内存页对齐，配置 `NDK_HOME` 与 `ANDROID_NDK_HOME`）。
- **Rust Android Targets**：
  ```bash
  rustup target add \
    aarch64-linux-android \
    armv7-linux-androideabi \
    i686-linux-android \
    x86_64-linux-android
  ```

> **说明**：iOS 客户端编译需在 macOS 环境下安装完整 Xcode、CocoaPods 及 Rust Apple iOS targets（`aarch64-apple-ios`、`aarch64-apple-ios-sim`、`x86_64-apple-ios`）。

### 3.4 Cargo 链接加速配置（Linux x86_64）

建议在 `$CARGO_HOME/config.toml`（或项目级 `.cargo/config.toml`）中为 Linux 主机目标启用 `lld`：

```toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
```

### 3.5 一键初始化脚本（Ubuntu 24.04 / 云端开发环境）

仓库内提供了幂等的非交互初始化脚本 [`scripts/setup-env.sh`](../scripts/setup-env.sh)，并已接入 [`.agents/environment.json`](../.agents/environment.json) 供云端开发机自动执行：

```bash
bash scripts/setup-env.sh
```


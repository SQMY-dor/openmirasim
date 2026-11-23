# OpenMirasim 路线图与协作分工

本文档定义 OpenMirasim 各阶段的**可复现 Demo 验收清单**，以及 **hark（Opus 5.5）** 与 **Antigravity** 的模块分工、并行边界与质量门禁规范。

---

## 1. 分阶段路线与 Demo 验收清单

### Prep：决议落盘与工程底座（已完成本 PR 范围）
- [x] 回写 [`design-review.md`](design-review.md) R1–R7 决议并同步更新 [`architecture.md`](architecture.md) 与 [`tech-stack.md`](tech-stack.md)
- [x] 修复 [`scripts/setup-env.sh`](../scripts/setup-env.sh)（补 `clang`/`bubblewrap`、`--cloud`/`--desktop`/`--android` 拆分、移除默认 Buf/protoc、不污染本机全局配置）
- [x] 初始化 3-crate Cargo Workspace（`om-core` / `om-server` / `om-cli`）、`apps/web`、`.cargo/config.toml`、`rust-toolchain.toml` 与最小 GitHub Actions CI

### Phase 0：纵向切片 MVP（核心闭环）
**可复现 Demo 验收标准**：
- [x] 启动单台 Linux `om-server`（静态 Token 鉴权、单 Space、`rusqlite` WAL 数据库）
- [x] 在 Web 页面登录后创建会话，支持 `MockHarness`、`om-fake-acp-agent` 与原生 **`opencode acp`**（已实测 `opencode 1.19.6` `protocolVersion=1` & `loadSession=true`）发送 Prompt 并实时看到流式文本与工具调用事件
- [x] 对话进行中**刷新浏览器或断开网络后恢复**，前端携带 `last_seq` 自动补齐缺失事件，零上下文丢失
- [x] 重启 `om-server` 进程后，Web 页面仍可列出历史会话、回放完整历史事件并支持一键恢复会话继续对话
- [x] 自动化 E2E 测试（Playwright + 真实 `om-server` 二进制 + SQLite WAL 重启恢复）在 CI 中稳定通过

### Phase 1：Linux 裸金属隔离 + 审批闭环 + 多 CLI Agent + `om-cli ctl`
**可复现 Demo 验收标准**：
- [x] 为会话自动创建隔离 Git Worktree，Agent 在 **`bubblewrap` (`bwrap`) + `systemd-run`/cgroups v2** 沙箱内执行（含 PR #29 每会话独立仓库与只读 `.git` 隔离加固，CI `OM_REQUIRE_BWRAP=1` 验收通过）
- [x] Agent 发起高危工具调用时触发 `ApprovalRequest`，Web 页面弹出审批卡片，审批通过后会话继续执行；并发重复审批被原子拒绝，并支持记住/清空会话级审批规则
- [x] 在 Web 页面注册 Git 仓库并查看当前会话 Worktree 的文件树、源码预览与实时 Git Diff
- [x] 完整接入 4 个无头 CLI Agent：**`opencode`**、**`dsh`**（DeepSeek Harness）、**`cc`**（Claude Code）、**`codexcli`**（Codex CLI），支持启动前版本与能力探测（`/api/v1/harnesses/probe`）
- [x] 通过 `om-cli ctl` 以纯 JSON 完成健康检查、仓库/会话/Worktree/Diff/队列查询、发 Prompt 与命令行审批

### Phase 2：30 人团队协作 + 密钥隔离 + PWA 推送 + Antigravity GUI 桥接
**可复现 Demo 验收标准**：
- [x] 支持 30 人以内团队成员管理、`Owner/Admin/Member/Viewer` 角色权限校验、设备 Token（`omt_...`）签发/吊销与操作审计日志（服务端 RBAC + Web 团队控制台 + `om-cli ctl members/tokens`）
- [x] 宿主机本地鉴权代理生效：沙箱内执行 `env` 看不到真实上游模型 API Key，仅持有短期 `ost_...` 会话令牌且 BYOK 调用与统计接口正常完成
- [x] 多成员同时打开同一会话可实时同步输出，支持 `Queue & Steer`（排队、插队、撤回、成员归属；不支持原生 Steer 的 Harness 自动降级）
- [x] 移动端浏览器以 **PWA**（`manifest.webmanifest` + `sw.js`）安装后支持 **Web Push / Service Worker 审批提醒**并可直接点击放行/拒绝（含 SSRF 校验与订阅管理 API）
- [x] **Antigravity GUI 独立桥接原型（`om-gui-bridge`）**：以独立外部进程接入（Linux `Xvfb` + 1 成员 1 独立 `0700` Profile 隔离 + 单机并发上限配额与空闲自动回收），支持消息转发与一键切入 noVNC 远程接管画面（不绕过登录/CAPTCHA，安全引导人工接管）

### Phase 3：桌面薄壳 + PTY 原生终端 + Ratatui TUI
**可复现 Demo 验收标准**：
- [ ] 独立 `/ws/pty/:session_id` Binary 流 + `portable-pty` + 内存环形缓冲 + **asciicast v2（`.cast`）** 录制与回放，Web 前端内嵌 `xterm.js` 切换聊天/终端视图且不阻塞 `/ws/events`
- [ ] **Tauri v2 桌面薄壳**（Linux / macOS / Windows）：连接中心服务器、系统托盘、原生通知、OS Keychain 保存 Token、多服务器 Profile 切换
- [ ] `om-cli tui`（Ratatui）：无头 Linux 服务器终端内查看多会话并直连 PTY

### Phase 4：编排扩展与按需演进
**可复现 Demo 验收标准**：
- [ ] Space / Workspace 两级 MCP Hub 与 Skills 生命周期管理
- [ ] Schedules & Goals 定时任务与目标驱动循环、Session-to-Session 协同派发
- [ ] 视实际社区/第三方需求评估是否引入 Protobuf SDK 与原生 Tauri Mobile 客户端
- [ ] 公开推广前完成品牌名/商标合规复核

---

## 2. 双 Agent 协作分工方案（hark / Opus 5.5 vs Antigravity）

### 2.1 角色定位与核心职责

| 角色 | 优势区间 | 负责主线 |
| --- | --- | --- |
| **hark（Opus 5.5）** | 复杂 Rust 并发状态机、类型契约设计、协议解析、Linux 安全沙箱边界与深度 Code Review | **服务端核心内核与安全边界**：`om-core` 契约、`rusqlite` 存储与事务、通用 `AcpHarness`、`SessionOrchestrator` 状态机、`bwrap`/cgroups 沙箱、Git Worktree、审批引擎、RBAC 与宿主机 Key 代理、关键 PR 审查 |
| **Antigravity** | 全栈前端/PWA/Tauri 交互、多外部 CLI/GUI 实测联调、工程基建与云端自动化 E2E 验证 | **外围集成、前端多端与质量验证**：文档与 `setup-env.sh`、Monorepo/CI 底座、Axum REST/WS 接入层、具体 Agent 适配（`Mock`/`opencode`/`dsh`/`cc`/`codexcli`/`om-gui-bridge`）、`apps/web` + PWA、Tauri 桌面壳、`om-cli` / TUI、PTY + `.cast` + `xterm.js`、全链路与对抗性测试 |

### 2.2 分阶段任务归属表

| 阶段 | 任务包 | 模块 / 目录 | 主责方 | 协同 / 审查方 |
| --- | --- | --- | --- | --- |
| **Prep** | 文档决议回写、`setup-env.sh` 修复、3-crate + Web + CI 底座 | `docs/`、`scripts/`、`.github/`、根配置 | **Antigravity** | hark Review |
| **Phase 0** | W0-1 `om-core` 领域模型、事件协议、`ts-rs` 导出、`Harness` trait | `crates/om-core/` | **hark（Opus 5.5）** | Antigravity 验证 TS 生成 |
| **Phase 0** | W0-2 `rusqlite` WAL + 单写线程 + Schema 迁移 | `crates/om-server/src/db/` | **hark（Opus 5.5）** | Antigravity 跑重启恢复测试 |
| **Phase 0** | W0-3 通用 `AcpHarness`（JSON-RPC 2.0 stdio + 权限回调） | `crates/om-server/src/acp/` | **hark（Opus 5.5）** | Antigravity 实测 `opencode acp` |
| **Phase 0** | W0-4 `SessionOrchestrator` Actor 与 `seq` 事件广播 | `crates/om-server/src/orchestrator/` | **hark（Opus 5.5）** | Antigravity 跑断线续传测试 |
| **Phase 0** | W0-5 Axum REST + `/ws/events` 续传握手 + Token 鉴权 | `crates/om-server/src/api/` | **Antigravity** | hark Review |
| **Phase 0** | W0-6 `MockHarness` + `opencode` 启动封装 | `crates/om-server/src/harness/` | **Antigravity** | hark Review |
| **Phase 0** | W0-7 Web 工作台 MVP + W0-8 Playwright E2E 验收 | `apps/web/`、`tests/e2e/` | **Antigravity** | hark Review |
| **Phase 1** | W1-1 `bwrap` + cgroups v2 沙箱、W1-2 Git Worktree、W1-3 审批状态机 | `crates/om-server/src/{sandbox,workspace,approval}/` | **hark（Opus 5.5）** | Antigravity 写逃逸/并发回归测试 |
| **Phase 1** | W1-4 `dsh`/`cc`/`codexcli` 适配、W1-5 Diff/审批 UI、W1-6 `om-cli ctl` | `crates/om-server/src/harness/`、`apps/web/`、`crates/om-cli/` | **Antigravity** | hark Review |
| **Phase 2** | W2-1 30 人 RBAC 与审计、W2-2 宿主机 Key 鉴权代理、W2-3 Queue & Steer | `crates/om-server/src/{auth,proxy,orchestrator}/` | **hark（Opus 5.5）** | Antigravity 跑安全与多人并发 E2E |
| **Phase 2** | W2-4 团队 UI + PWA Web Push、W2-5 `om-gui-bridge`（Antigravity GUI） | `apps/web/`、`crates/om-gui-bridge/`（外置） | **Antigravity** | hark 审隔离边界与配额策略 |
| **Phase 3** | PTY 运行时（`.cast` + `/ws/pty`）+ `xterm.js` + Tauri 桌面薄壳 + Ratatui TUI | `crates/om-server/src/pty/`、`apps/{web,tauri}/`、`crates/om-cli/` | **Antigravity** | hark 审背压与进程资源回收 |
| **Phase 4** | Schedules/Goals 调度与 Session-to-Session 权限路由 | `crates/om-server/src/{scheduler,orchestrator}/` | **hark（Opus 5.5）** | Antigravity 补测试与管理 UI |
| **Phase 4** | MCP Hub / Skills 管理界面与生态集成 | `apps/web/`、`crates/om-cli/` | **Antigravity** | hark Review |

---

## 3. 并行防冲突与协作流程规范

1. **目录所有权隔离（避免 Merge Conflict）**：
   - **hark（Opus 5.5）主改目录**：`crates/om-core/`、`crates/om-server/src/{db,acp,orchestrator,sandbox,workspace,approval,auth,proxy,scheduler}/`
   - **Antigravity 主改目录**：`docs/`、`scripts/`、`.github/`、`apps/`、`crates/om-cli/`、`crates/om-server/src/{api,harness,pty}/`、`tests/`
   - 跨目录公共入口（如 `crates/om-server/src/main.rs`、根 `Cargo.toml`）通过小步 PR 合并，尽量不在同一时刻双边大改。
2. **契约门禁（Contract-First Gate）**：
   - 每个 Phase 先合入 hark 的 **`om-core` 类型与 Trait 契约 PR**（包含 `ts-rs` 导出的 TS 定义），确认通过 CI 后，双方再按目录边界并行开发。
3. **交叉验证闭环**：
   - **hark 负责核心正确性与安全底线**：对涉及接口、并发、权限与外部接入的 PR 做深度审查；
   - **Antigravity 负责端到端可运行性**：在云端环境运行真实编译、`cargo clippy`、`cargo test`、Playwright E2E 与沙箱越界测试，确保每个阶段交付物满足 Demo 验收标准。

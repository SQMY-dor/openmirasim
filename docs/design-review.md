# 架构与技术栈评审意见（待讨论）

> 状态：**提案 / Discussion**。本文档是对 [`architecture.md`](architecture.md) 与 [`tech-stack.md`](tech-stack.md) 的评审意见，尚未修改原有设计。每条意见带编号，讨论后在“决议”一栏记录结论，再回写到正式文档。

## 总体判断

架构核心思路是对的：单中心服务器、事件流 `last_seq` 断点续传、Git Worktree 隔离、审批门控，这几件事就是这个产品的价值所在，应保留。

主要问题是**范围与复杂度超出 MVP**：四种客户端（Web / Tauri Desktop / Tauri Mobile / CLI-TUI）+ 双传输协议（gRPC + HTTP/WS）+ 团队 RBAC 在设计上同时铺开，而仓库还没有一行业务代码。建议先用一条能跑通的纵向切片验证核心闭环，再逐层加能力。

---

## R1. 缩小范围：Phase 0 纵向切片

当前 Phase 1 即包含 proto + server + PTY + 多 Harness + Web + CLI。建议新增 **Phase 0**，验收标准：

- [ ] 一个 Space、一个用户（静态 Token 登录）
- [ ] 接入一个 Agent（Claude Code 或 Gemini CLI，见 R3）
- [ ] Web 页面可发送消息、查看流式输出
- [ ] 浏览器刷新 / 断网后按 `last_seq` 恢复完整上下文
- [ ] 会话与事件持久化到 SQLite，服务端重启后可列出历史会话

**移动端**：Phase 3 之前不做 Tauri Mobile。将 Web 工作台做成 **PWA + Web Push**，手机端查看进度与审批卡片即可满足需求，省去 Android NDK / iOS / Xcode 整条工具链。

**桌面端**：Tauri 先只做连接服务器的薄壳（托盘、原生通知、Keychain、多服务器 Profile），不在壳内承载业务逻辑。

| 决议 | |
| --- | --- |

## R2. 协议层：Phase 0–2 暂不引入 gRPC + Buf

- 浏览器**无法直接使用原生 gRPC**，需要 grpc-web 或 Connect 代理；Web 工作台又是主力客户端，结果是同一套 API 要维护 Tonic gRPC 与 Axum HTTP/WS 两条传输路径。
- Rust 端 Connect 协议生态尚不成熟。
- Protobuf + Buf + protoc + 双端代码生成在早期会显著拖慢迭代。

建议：

- **只用 Axum：REST（控制面）+ 单条 WebSocket（事件流 / PTY 流）**。
- 以 **Rust 类型为单一事实源**，通过 [`ts-rs`](https://github.com/Aleph-Alpha/ts-rs) 或 [`specta`](https://github.com/specta-rs/specta) 生成 TypeScript 类型；CLI 直接复用 Rust crate。
- 事件统一用带标签的 JSON（`#[serde(tag = "type")]`），便于调试、抓包与脚本消费。
- 当需要第三方多语言 SDK 或外部集成时，再引入 Protobuf（可从 Rust 类型反推 schema）。

| 决议 | |
| --- | --- |

## R3. Agent 接入：结构化协议优先，PTY 兜底

当前 Harness 设计以 PTY 为主。从 TUI 字节流中解析“聊天视图、工具调用、审批请求”非常脆弱，上游 CLI 一升级就可能失效。

建议优先级：

1. **ACP（Agent Client Protocol，Zed 发起）**：Gemini CLI 原生支持；Claude Code、Codex 有 ACP 适配器；OpenCode 有 ACP 模式。ACP 本身包含**工具调用权限请求**，可直接映射到本项目的 `APPROVAL_REQUEST`。（开工前需再核对各适配器当前版本与能力。）
2. 各 CLI 的 JSON 流式输出模式（如 Claude Code `--output-format stream-json`、Codex 的 JSON 事件输出）。
3. **PTY**：只作为“原生终端视图”以及不支持结构化协议的 Agent 的兜底。

抽象一个 `Harness` trait：

```rust
#[async_trait]
pub trait Harness: Send + Sync {
    async fn start(&self, cfg: SessionConfig) -> Result<SessionHandle>;
    async fn send(&self, h: &SessionHandle, prompt: Prompt) -> Result<()>;
    async fn steer(&self, h: &SessionHandle, msg: Prompt) -> Result<()>;
    async fn cancel(&self, h: &SessionHandle) -> Result<()>;
    async fn resume(&self, native_session_id: &str) -> Result<SessionHandle>;
    fn events(&self, h: &SessionHandle) -> BoxStream<'static, HarnessEvent>;
}
```

先实现 `AcpHarness`，再实现 `PtyHarness`。

| 决议 | |
| --- | --- |

## R4. 安全：Worktree 不是隔离边界，需要执行沙箱

多人共享一台服务器运行 Agent，等价于团队成员都能在服务器上执行任意代码。当前文档只有 RBAC + 审批，缺少**执行隔离**：

- 每个会话以独立 Linux 用户 / bubblewrap / 容器（Docker 或 Podman，rootless 优先）运行。
- 网络出口白名单；cgroups 限制 CPU / 内存 / 进程数。
- 密钥仅以环境变量形式注入沙箱，**不得**写入事件流、日志或 Transcript。
- 审批策略与沙箱策略联动：沙箱内的写操作可自动放行，越界才需审批。

**凭证合规**：Claude / ChatGPT 等订阅账号多人共用通常违反服务条款。团队场景默认采用 **BYOK**（个人或团队 API Key），开源项目不提供共享订阅或反向代理类功能。

| 决议 | |
| --- | --- |

## R5. 存储：PTY 原始字节不进 SQLite

- SQLite（WAL）+ 单写 Actor 适合元数据与结构化事件，保留。
- PTY 原始帧量大、写入频繁：内存环形缓冲 + 按会话写 **asciicast v2（`.cast`）文件**，SQLite 只存索引与偏移。
- 结构化事件表直接用 `events(session_id, seq, ts, type, payload)`，主键 `(session_id, seq)`，无需另起独立 Event Log 组件。
- `sqlx` 与 `rusqlite` 二选一：需要编译期 SQL 校验选 `sqlx`；偏好简单、同步写串行选 `rusqlite` + 专用写线程。建议尽早定下，避免两套并存。

| 决议 | |
| --- | --- |

## R6. `scripts/setup-env.sh` 的具体问题

1. **缺少 `clang`**：apt 列表有 `lld` 但没有 `clang`，第 7 步检测不到 clang 会静默跳过，lld 加速在干净 Ubuntu 上实际不生效。
2. **删除 `~/.bashrc` 中的 `case $- in ... esac` 块**：会改变交互式 shell 默认行为，不应出现在通用脚本中；至少限定仅在 CI / 云端开发机执行（如检测 `CI` 环境变量）。
3. **写入全局 `$CARGO_HOME/config.toml`**：应改为仓库内 `.cargo/config.toml`，不污染开发者本机配置。
4. **默认安装 Android NDK 与 4 个 Android target**：MVP 不需要，拆为 `setup-env.sh --android` 可选项。
5. **Buf 仅下载 `Linux-x86_64`**：ARM 主机会装错；若采纳 R2，此步骤可删除。
6. **缺少 Rust / Node / pnpm 安装与版本校验**：建议增加 `rust-toolchain.toml`，并在 `package.json` 中声明 `engines` 与 `packageManager` 固定版本。

| 决议 | |
| --- | --- |

## R7. 工程化补充

- 新增 `docs/roadmap.md`，把各 Phase 的验收标准写成可复现的 demo 清单。
- 最小 CI：`cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test`、`pnpm typecheck`。
- Crate 拆分先收敛为 `om-core`、`om-server`、`om-cli` 三个，`om-storage` / `om-runtime` 等在代码长出来后再拆。
- 项目名直接包含其他产品名（Mirasim），正式推广前需留意商标问题。

| 决议 | |
| --- | --- |

---

## 建议的新路线

| 阶段 | 内容 |
| --- | --- |
| Phase 0 | Axum REST + WS、SQLite 事件表、1 个 ACP Agent、Web 流式对话、断线续传、Token 登录 |
| Phase 1 | Worktree 隔离、执行沙箱、审批流、Diff 视图、CLI `ctl` |
| Phase 2 | 多成员 RBAC、Team Space、Queue & Steer、PWA 推送审批 |
| Phase 3 | Tauri 桌面壳、PTY 原生终端视图、Ratatui TUI |
| Phase 4 | MCP Hub、Skills、定时任务、会话间协作；按需引入 Protobuf / 原生移动端 |

## 待讨论的开放问题

1. 目标团队规模多大？（决定 SQLite 是否长期够用、RBAC 做多细）
2. 服务器部署形态：裸机 / Docker / 家用 NAS？（决定沙箱方案）
3. 首个接入的 Agent 选哪个？
4. 是否必须支持 Windows 服务器？（影响 PTY 与沙箱实现）

# OpenMirasim 全仓深度代码 Review 与修复追踪（`main @ 9726d2d`）

> **审查基线**：`origin/main` `9726d2d`（PR #1–#32，约 44.5k LOC / 259 个受控文件，涵盖 Phase 0–4 全栈：`om-core`、`om-server`、`om-gui-bridge`、`om-cli`、`apps/web`、`apps/tauri`、`scripts/`、`.github/workflows/` 与 `tests/e2e/`）。
> **推进方式**：按 **PR 1（后端 `fix/review-backend`）** / **PR 2（前端 `fix/review-frontend`）** / **PR 3（CLI + Tauri + Infra `fix/review-cli-tauri`）** 拆分为 3 个独立 PR 推进，各 PR 零文件冲突且独立通过全量 CI（`rust` + `OM_REQUIRE_BWRAP=1` / `web` / `fullstack-e2e`）。

---

## 1. 关键架构决策（随本轮修复一并落地）

1. **不同 Space 成员按会话隔离 `$HOME`**（PR 1）：
   - `WorkspaceManager::session_home` 将每个会话的 `$HOME` 隔离至 `homes/<space_id>/<member_id>/<session_key>`（匿名会话落盘于 `homes/_anonymous/<session_key>`），并在 `discard()` / `delete_checkout()` 时回收，彻底消除同成员或跨会话间凭据/配置污染。
2. **检测到上游 Provider API Key 时默认开启 `KeyProxy`**（PR 1）：
   - `om-server` 启动时自动检测 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `DEEPSEEK_API_KEY` / `OPENROUTER_API_KEY`；一旦存在即默认启用 loopback `KeyProxy` 并禁止向 Agent 透传明文密钥（仅 loopback 允许通过 `OM_ENABLE_KEY_PROXY=0` 显式关闭）。
3. **MCP 按成员隔离，管理员可控制哪些 MCP 全局可用**（PR 1 后端 + PR 2 前端 + PR 3 CLI）：
   - `Space` 级 MCP 仅 `Admin`/`Owner`（`ManageWorkspaces`）可增删改，并新增 `global_available: bool` 开关精确控制哪些 MCP 对全 Space 成员全局可见及生效；
   - `Workspace` 级 MCP 强制绑定 `owner_member_id`（`global_available = false`），各成员的 `list` / `probe` / `update` / `delete` / `hub/effective` 严格隔离，互不可见且同 Workspace 下允许不同成员使用同名 MCP。

---

## 2. 总体统计与修复状态

| 严重级别 | 数量 | PR 1（后端） | PR 2（前端） | PR 3（CLI / Tauri / CI） |
| --- | :---: | :---: | :---: | :---: |
| **P0（阻断级安全 / 逃逸 / 崩溃 / 越权）** | 14 | **10 项（已修复）** | 4 项（PR 2） | — |
| **P1（高危安全 / 竞态 / 资源泄漏 / 契约）** | 31 | **20 项（已修复）** | 5 项（PR 2） | 6 项（PR 3） |
| **P2（健壮性 / 性能 / 测试缺口）** | 22 | **已同步收口** | PR 2 | PR 3 |

---

## 3. P0 阻断级缺陷清单与修复明细（14 项）

| ID | 模块 | 问题摘要 | 修复归属 | 状态 |
| --- | --- | --- | --- | --- |
| **P0-1** | `pty/` + `api/pty_ws.rs` | `/ws/pty/{id}` 绕过 `BwrapSandbox` 与 `allow_direct_sandbox`，在宿主机直接 spawn 无隔离 shell（缺失 `cwd` 甚至回落 `/tmp`） | PR 1 | 已修复：接入 `Orchestrator::pty_sandbox` + `SandboxSpec::keep_session`，拒绝已结束会话与已删 worktree |
| **P0-2** | `workspace/mod.rs` | `objects/info/alternates` 虽只读挂载，但父目录 `objects/` 未设挂载点，沙箱内 `mv objects/info` 可重命名替换 `alternates` | PR 1 | 已修复：绑定挂载 `<gd>/objects` 使 `rename()` 返回 `EBUSY`，只读覆盖 `objects/info` 与 `alternates`，校验无 symlink 并传 `core.alternateRefsCommand=""` |
| **P0-3** | `workspace/mod.rs` | `diff()` 对未跟踪文件调用宿主机 `git diff --no-index`，TOCTOU 中间目录符号链接可导致宿主机任意文件读取 | PR 1 | 已修复：移除 `git diff --no-index`，改为 `spawn_blocking` 内用 `nofollow::read_truncated` + 进程内 unified diff 生成 |
| **P0-4** | `om-gui-bridge` | 绑定非 loopback 或运行 `X11` 时缺省无 `--token` 即裸奔开放 `/v1/*` 控制面与任意 GUI 启动 | PR 1 | 已修复：非 loopback 绑定与 `x11` 驱动无 `bridge_token` 强制 fail-closed 拒绝启动 |
| **P0-5** | `om-gui-bridge` | `x11vnc -nopw`、无 `-auth` Xauthority、子进程继承 `OM_GUI_BRIDGE_TOKEN`、GUI 进程无 `bwrap` 隔离、`Xvfb` 启动竞态 | PR 1 | 已修复：全子进程 `.env_clear()`，每会话生成 `0600` Xauthority 与随机 VNC `-passwdfile`，等待 X11 socket 就绪并优先用 `bwrap` 包裹 GUI |
| **P0-6** | `harness/{cc,codexcli}.rs` | `prompt` / `model` / `native_session_id` 未校验且缺 `--` 分隔符，`-` 开头输入可注入任意 CLI 危险参数 | PR 1 | 已修复：强制校验 `model`/`native_session_id` 白名单并过滤 Agent 回传的非法 ID；在 prompt 前统一追加 `--` |
| **P0-7** | `harness/{probe,opencode}.rs` | `probe_cli_version` / `probe_opencode` 未调用 `.env_clear()`，向探测子进程泄漏 `OM_TOKEN` 及全部宿主 API Key | PR 1 | 已修复：统一调用 `.env_clear()` + 最小安全变量 + 有界 stdout/stderr 捕获，去重 `probe_opencode` |
| **P0-8** | `approval/mod.rs` | 记忆拒绝规则（`!r.allow`）在 Agent 未提供 Reject 选项时穿透至自动放行规则 | PR 1 | 已修复：无 Reject 选项时 fail-closed 返回 `Verdict::Ask` |
| **P0-9** | `scheduler/mod.rs` | `Scheduler::update()` 与 `trigger()` 缺少所有权校验且 `trigger()` 绕过并发槽位，任意 Member 可劫持 Owner 定时任务 | PR 1 | 已修复：强制 `check_manages`、`fire_slots` 并发信号量与审计记录 |
| **P0-10** | `acp/harness.rs` | Agent stderr 输出非 UTF-8 字节时 `read_line` 中止读取循环，写满管道缓冲导致 Agent 永久死锁 | PR 1 | 已修复：改用 `read_line_capped(..., 64 KiB)` 逐行有损解码排空至 EOF |
| **P0-11** | `apps/web/src/App.tsx` | URL 参数 `?approve=&option=` 在加载时自动调用 `resolveApproval()`，存在点击/链接 CSRF 自动批准高危工具风险 | PR 2 | 计划于 PR 2 修复（改为深链聚焦审批卡片，需显式点击确认） |
| **P0-12** | `api/mod.rs` + `gui_bridge.rs` + `App.tsx` | `Viewer` 可读取含 `?k=` 密钥的 `takeover_url`（API + 事件流双重泄漏）并直接接管交互式 GUI | PR 1 + PR 2 | 后端（PR 1）已修复：聊天回复不再内嵌 `takeover_url`，`remote-screen` 对无 `PromptSession` 权限角色隐藏屏幕并脱敏 `profile_dir`；前端（PR 2）补 UI 门控与协议校验 |
| **P0-13** | `apps/web/.../TerminalView.tsx` | `.cast` 历史回放写入活跃交互式终端，回放期间实时 WS 输出交错且用户按键会发送至远端 PTY | PR 2 | 计划于 PR 2 修复 |
| **P0-14** | `apps/web/.../ConnectView.tsx` + `App.tsx` | 切换服务器 Profile 时未立即清空旧 `token` 导致跨服凭据误发；Tauri 桌面版仍明文落盘 `localStorage` | PR 2 | 计划于 PR 2 修复 |

---

## 4. P1 高危缺陷清单与修复明细（31 项）

### 4.1 PR 1 后端已修复（P1-1 ~ P1-20）

- **P1-1 (`sandbox/mod.rs`)**：追加 `--remount-ro /` 防止写爆根 tmpfs；剥离宿主 `XDG_*`；拒绝 `writable_paths`/`home`/`readonly_paths` 为符号链接；过滤 `PATH` 仅保留绝对路径；修复 `is_covered` canonicalize 校验。
- **P1-2 (`sandbox/mod.rs`)**：修复 `BwrapSandbox::probe()` 遇 `--size` 后未继续递归回退 `--disable-userns` 的问题。
- **P1-3 (`workspace/{mod,nofollow}.rs`)**：`list_files()` 改为 `git.run_capped` + `nofollow::stat_non_dir` 放入 `spawn_blocking`；根目录 open 补 `O_NOFOLLOW`、`max.saturating_add(1)`、封顶 `walk_dir` 遍历；落地按 `(space, member, session_key)` 隔离 `$HOME` 与清理。
- **P1-4 (`proxy/mod.rs`, `main.rs`)**：降低 `max_body_bytes` 默认值至 8 MiB 并加全局并发体缓冲上限；先校验单会话并发再抢全局槽位；0 Provider 时不强制 `spec.network = true`；`main.rs` 检测到上游 Key 时默认开启 `KeyProxy` 并对 `Direct` 会话也套用 `ProxyEnvSandbox`。
- **P1-5 (`pty/{mod,cast}.rs`, `api/pty_ws.rs`, `api/mod.rs`)**：拆出 `openpty`/`fork` 到 `spawn_blocking` 且不持锁；进程组 `SIGHUP`+`SIGKILL` 清理；检查线程启动错误；持 `ring` 锁完成快照+订阅及推送+广播防丢/重帧；`CastWriter` 增 3 字节 UTF-8 跨块续接缓冲与 `O_NOFOLLOW`；`get_pty_recording` 改为异步流式传输；限制 WS 帧大小并上报本地 `Lagged`。
- **P1-6 (`approval/mod.rs`)**：记忆放行规则仅选 `AllowOnce`/`RejectOnce` 且强制 `SandboxMode::Bwrap`；`all_within` 拒绝 `cwd == "/"`、`.git` 路径及 `n == root`。
- **P1-7 (`auth/mod.rs`, `db/members.rs`, `api/auth.rs`, `om-core/src/auth.rs`)**：`IssuedToken` 手动 `Debug` 脱敏；`Authorize` 携带 `token_id` 并在写事务内原子校验调用方 Token 活性、裁切子 Token TTL 上限及自动清理过期/撤销记录；保留 `CoreError::Storage` 不误报 `Forbidden`；修复 `OM_TOKEN` 以 `omt_` 开头时的鉴权回退。
- **P1-8 (`auth/mod.rs`, `api/ws.rs`, `api/pty_ws.rs`)**：新增 `Auth::verify_principal`，在 `/ws/events` 与 `/ws/pty/{id}` 周期及每次 `Subscribe`/写输入前重验 Token/成员状态；补齐 WS `Origin` 校验、帧大小上限、订阅数上限（64）与可靠 `Resync` 投递。
- **P1-9 (`api/push.rs`, `api/mod.rs`, `main.rs`)**：修复 SSRF 校验支持方括号 IPv6、`to_ipv4_mapped()`、link-local/CGNAT/reserved/metadata 与 `localhost.`/`*.localhost`；字段限长、唯一临时文件 `0600`+`sync_all` 原子落盘并向上传播错误；列表接口隐藏 `keys.auth`；`/push/test` 如实回报 `delivered: 0`；`allow_loopback_push` 改为显式 `OM_ALLOW_LOOPBACK_PUSH=1`；`main.rs` 移除 `OM_FAKE_AGENT_BIN` 环境变量继承并支持可选 cgroups 配置。
- **P1-10 (`api/hub.rs`, `api/mod.rs`)**：落地成员隔离 MCP + 管理员 `global_available` 控制；屏蔽 `OM_*`/`LD_*`/`DYLD_*`/`PATH`/模型 Key 等敏感 `env_keys`；HTTP/SSE MCP 套用 SSRF 校验；Stdio 命令禁止 `..` 与非可执行文件并脱敏探测路径；`effective()` 排除 `Draft` Skill；先验 RBAC 再查 ID；先克隆持久化再提交内存。
- **P1-11 (`om-core/src/{schedule,goal}.rs`, `db/{mod,schedules,goals,audit}.rs`)**：修复 `Weekday::of_epoch_day` 溢出、`weekdays` 长度与去重校验、`Daily::next_after` 全 checked 算术；`Goal::is_met_by` 忽略围栏代码块；`NewGoal::validate` 限制 `deadline_at <= 366 天`；DB 初始化顺序修正、并发读连接 `Semaphore` 限流、`catch_unwind` 防写线程静默失踪、区分 `UNIQUE`/`FOREIGNKEY`、`set_schedule_enabled`/`record_schedule_run` 事务化与迁移 v8（`dispatches.from_turn_id`）。
- **P1-12 (`acp/{rpc,harness}.rs`)**：`in_tx` 改有界队列（512），EOF 与超长行经 `in_tx` 因果保序失败所有 `pending`，超长行直接终止避免死锁；新增 `PendingGuard`；`loading` 期间拒收审批请求；限制 `tool_titles` 内存与空标题；退出等待 5s 超时强杀。
- **P1-13 (`orchestrator/{actor,mod,hub}.rs`)**：`actor::run` 补 `ExitGuard` 兜底清理；修复 `start_harness` 取消安全与启动事件上限；`on_move` 先落库再改内存；关闭/崩溃补发 `QueuePaused { paused: false }`；`TurnEnded` 清空 `tool_calls`；`close_session` 处理 actor 已退出回落；`create_session` 失败回滚清理；`EventHub` 用 `Weak` + `Drop` 回收通道并改为按页懒加载。
- **P1-14 ~ P1-16 (`scheduler/{mod,goals,dispatch}.rs`)**：`Scheduler` 引用计数持有 `starting` 跨越 `record(run)`、活跃子会话去重与 1s 错误退避；`GoalRunner` 随机不可预测 `done_marker`、`StopsGuard`、安全 `checked_add` deadline、新鲜 `head_seq`、256 KiB 回复尾缓冲、失败回合不判达成；`Dispatcher` 多成员 `CreateSession` 全校验、匿名跨成员路由、基于 `from_turn_id` 的扇出与 `DispatchGuard`。
- **P1-17 (`api/mod.rs`)**：全站安全响应头（`X-Content-Type-Options`/`X-Frame-Options`/`Referrer-Policy`/`CSP frame-ancestors 'none'`/`Cache-Control: no-store`）；`probe_harnesses` 30s 缓存与非管理员路径脱敏；`remove_worktree` 拒绝删除活跃会话/PTY 的 worktree；`resolve_approval` 隐藏跨 Space `SessionId`。
- **P1-18 (`harness/stream_cli.rs`, `cc.rs`, `codexcli.rs`)**：stderr 排空至 EOF（16 KiB 尾缓冲）；不在 `child.wait()` 持 `active_child` 锁；每回合独立取消标志与 `Drop` 清理；修复 `line_overflow` 死代码、Claude 流式重复文本与 Codex 孤儿 `ToolCallUpdate`。
- **P1-19 (`om-gui-bridge`, `harness/gui_bridge.rs`)**：全量消除 `Mutex::lock().unwrap()`；进程启停移出锁外并回收子进程后再释放 display；`xdotool` 会话级串行 + 15s 超时；256-bit CSPRNG `takeover_token`、安全响应头与服务端存储 `novnc_port`；`gui_bridge.rs` 按 `created_by` 隔离成员 profile、禁重定向、校验 `bridge_session_id`/`takeover_url` 与防重复 `TurnEnded`。
- **P1-20 (`harness/{probe,opencode}.rs`)**：从 `SAFE_BASE_ENV` 移除 `XDG_*`；`OpenCodeConfig::from_env_for_proxy()` 不再透传 `OPENCODE_CONFIG_CONTENT`。

### 4.2 PR 2 前端待修（P1-28 ~ P1-31 + P0-11 ~ P0-14）
- **P0-11 ~ P0-14**：移除 URL 自动审批、补齐 `remote-screen` 角色与协议门控、隔离 `.cast` 回放态、修复 Profile 切换 Token 泄漏与 Tauri `localStorage` 明文存储。
- **P1-28**：WS `Subscribed` 假连接态修复、Pong 超时、`last_seq > head_seq` 重置、防 React 19 updater 副作用与序号跳变强制重同步。
- **P1-29**：`Composer` 在队列非空时带 `mode` 提交、按会话重置草稿、防连点双发与仅成功时清空。
- **P1-30**：实现 `disableApprovalPush` 退订清理；修复 `sw.js` 缓存新鲜度与消息来源校验。
- **P1-31**：`TeamConsole` / `HubConsole`（接入成员隔离与管理员全局可用开关）/ `AutomationConsole` / `WorktreeDrawer` / `NewSessionModal` / `SessionStream` / `MarkdownContent` 竞态与边界修复。

### 4.3 PR 3 CLI / Tauri / Infra 待修（P1-21 ~ P1-27）
- **P1-21**：`om-cli` 改用 `Url::path_segments_mut` 编码路径参数防注入，支持 `--global-available` 与 `--token-file`。
- **P1-22**：`Cargo.toml` 为 `tokio-tungstenite` 启用 `rustls-tls-webpki-roots`；`tui.rs` 改用 `Authorization: Bearer` 握手头。
- **P1-23**：`tui.rs` 退出防阻塞、RAII 终端恢复 + panic hook、审批选 `AllowOnce`/`RejectOnce` + 队列 + 事件 `session_id` 过滤与断线重连。
- **P1-24**：`ctl.rs` `schedules set-enabled` / `mcp set-enabled` 补 `ArgAction::Set`。
- **P1-25**：`apps/tauri` 迁移 `profiles.json` 出 `/tmp` 到 `app_config_dir()` + `0700`/`0600` + `Mutex` + `sync_all`；清理淘汰 Keychain；校验 `server_url` 协议；收紧 CSP；阻塞操作移入 `spawn_blocking`。
- **P1-26 / P1-27**：`scripts/setup-env.sh` 固定 `cargo-binstall` 版本与 SHA-256 校验、移除破坏性 `sed`；`ci.yml` 补 `permissions: { contents: read }` 与 Tauri 构建检查。

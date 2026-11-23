# OpenMirasim 全仓深度代码 Review 报告（`main @ 2a53441`）

> **审查基线**：`origin/main` `2a53441`（PR #1–#26，涵盖 Phase 0 闭环及 hark 已合入的 W1-1 `sandbox`、W1-2 `workspace`、W1-3 `approval`、W2-1 `auth`、W2-2 `proxy`、W2-3 `queue & steer`、W4-1 `schedules / goals / dispatch`）。
> **目录归属**：按 [`docs/roadmap.md`](roadmap.md) 约定，hark 主责 `crates/om-core/` 与 `crates/om-server/src/{db,acp,orchestrator,sandbox,workspace,approval,auth,proxy,scheduler}/`；Antigravity 主责 `docs/`、`scripts/`、`.github/`、`apps/`、`crates/om-cli/`、`crates/om-server/src/{api,harness,pty}/`、`tests/`。

---

## 1. 总体结论

1. **核心库质量扎实**：单写线程 SQLite WAL + `WITHOUT ROWID` 事件表、OS CSPRNG + SHA-256 哈希 Token、KeyProxy 禁止重定向与脱敏日志、调度 CAS 抢占与 `goals_one_active` 部分唯一索引均设计得很干净。
2. **存在 9 项 P0 级阻断缺陷（8 项在 hark 核心目录、1 项在 Antigravity 目录）**：包含共享 `.git` 可写挂载导致的宿主机 Git RCE、`Db::update_schedule` 漏写 `created_by` 提权、`KeyProxy` AB-BA 死锁与基路径穿越、跨 Space 审计泄漏、审批通配绕过、ACP 永久死锁与通知/响应乱序、前端静默丢事件与 WS 日志泄漏 Token。
3. **接入现状**：W1-1～W4-1 核心库目前仅停留在 Rust 单元/集成测试层，尚未接入 `main.rs`、`api/`、`apps/web/` 与 `om-cli/`。Antigravity 侧的 P0/P1 修复已随本 PR 一并落盘，随后将推进 Phase 1 接入与适配。

---

## 2. P0：阻断级安全、死锁与静默丢数据问题（9 项）

### [P0-1] 共享 `.git` 可写挂载导致沙箱逃逸、宿主机 RCE 与跨会话污染（hark · W1-2）
- **位置**：[`workspace/mod.rs:429-455`](../crates/om-server/src/workspace/mod.rs#L429-L455)、[`workspace/git.rs:23-36`](../crates/om-server/src/workspace/git.rs#L23-L36)、[`sandbox/mod.rs:350-358`](../crates/om-server/src/sandbox/mod.rs#L350-L358)
- **根因**：`WorkspaceManager::sandbox_paths` 将宿主机整个共享仓库 `git_common_dir`（`<repo>/.git`）**读写挂载**进每个会话的 `bwrap` 沙箱，仅按黑名单用 `if p.exists()` + `--ro-bind-try` 覆盖部分文件：
  1. **宿主机 RCE（缺失的 `common/commondir`）**：常规仓库默认没有 `<repo>/.git/commondir`，`if p.exists()` 与 `--ro-bind-try` 会跳过它。沙箱内 Agent 可创建 `<repo>/.git/commondir` 指向 worktree 内伪造目录并写恶意 `config`（如 `filter.<name>.clean`/`smudge`/`process`，不在 `HARDENING` 禁用列表内），后续宿主机执行 `Git::at`（`prepare`/`remove_worktree`）即在宿主机沙箱外执行任意命令。
  2. **跨会话宿主机 RCE**：只叠加了当前会话的 `admin/{commondir,gitdir,config.worktree}`，其他会话的 `<common>/worktrees/<other>/` 全部可写。会话 A 篡改会话 B 的 `commondir` 或 `config.worktree` 后，宿主机对会话 B 调用 `diff()`/`list_files()` 即在宿主机执行恶意命令。
  3. **仓库/分支无隔离**：`<common>/refs/`、`HEAD`、`index`、`objects/`、`modules/*` 均对所有会话可写，任意会话可篡改默认分支、破坏对象库或覆盖他人分支。
- **修复建议**：
  - 不应将共享宿主机 `<repo>/.git` 可写挂载进沙箱。改为每个会话使用独立本地 clone（或独立 `.git` 目录）并通过加固的服务端 `git fetch` 回传提交；或将 `.git` 严格只读挂载、提交由服务端受控接口完成。
  - 纵深防御：[`sandbox/mod.rs:355`](../crates/om-server/src/sandbox/mod.rs#L355) 对显式 `spec.readonly_paths` 改用 fail-closed 的 `--ro-bind`（仅 `DEFAULT_SYSTEM_PATHS` 保留 `--ro-bind-try`）。

### [P0-2] 定时任务编辑提权：`Db::update_schedule` 漏写 `created_by`（hark · W4-1）
- **位置**：[`scheduler/mod.rs:122-149`](../crates/om-server/src/scheduler/mod.rs#L122-L149)、[`db/schedules.rs:105-131`](../crates/om-server/src/db/schedules.rs#L105-L131)
- **根因**：`Scheduler::update` 明确要求编辑者成为新所有者（`created_by: by.clone()`），但 `Db::update_schedule` 的 SQL `UPDATE schedules SET ...` **漏掉了 `created_by`** 却返回了内存中的 `Ok(s)`。后续 `due_schedules()` 仍以原 Owner 身份触发被编辑后的任意 Prompt/会话配置，形成身份劫持与权限提升。
- **修复建议**：SQL 补上 `created_by = ?10`，并在 [`tests/schedules.rs`](../crates/om-server/tests/schedules.rs) 补充跨成员编辑回归测试。

### [P0-3] `KeyProxy` 锁顺序反转导致 AB-BA 永久死锁（hark · W2-2）
- **位置**：[`proxy/mod.rs:328-329`](../crates/om-server/src/proxy/mod.rs#L328-L329)、[`348-349`](../crates/om-server/src/proxy/mod.rs#L348-L349)、[`357-359`](../crates/om-server/src/proxy/mod.rs#L357-L359)
- **根因**：`grant()`/`revoke_session()` 先锁 `grants` 再锁 `by_session`；`stats()` 先锁 `by_session` 再锁 `grants`。并发调用即可能死锁，并因 `lookup()` 需要 `grants` 锁而卡死全服所有代理请求。
- **修复建议**：合并 `grants` 与 `by_session` 到单个 `Mutex<GrantStore>`。

### [P0-4] `KeyProxy` 基路径穿越与 Query/Fragment 注入绕过 `safe_rest`（hark · W2-2）
- **位置**：[`proxy/mod.rs:451-459`](../crates/om-server/src/proxy/mod.rs#L451-L459)、[`487-504`](../crates/om-server/src/proxy/mod.rs#L487-L504)、[`217-224`](../crates/om-server/src/proxy/mod.rs#L217-L224)
- **根因**：请求 `/p/%64eepseek/...` 时 Axum 解码出 `provider_id = "deepseek"`，但 `raw_path.strip_prefix("/p/deepseek")` 为 `None`，`.unwrap_or(&rest)` 退回使用已百分号解码的 `rest`。`safe_rest()` 未拦截 `\t`/`\r`/`\n`/`?`/`#`，导致 `.%09.` 解码为 `.\t.` 绕过 `!= ".."`，传入 `reqwest`/`url::Url::parse` 后 WHATWG 规范剥离 `\t` 变成 `..`，携带真实上游 API Key 逃逸出基路径；`%3f`/`%23` 同理可注入 query 或截断路径。
- **修复建议**：`strip_prefix` 失败直接 `400` 禁止回退解码路径；拒绝一切控制/空白字符与 `?`/`#`；最终使用 `Url::parse` 解析上游完整 URL 并二次校验 `scheme`/`host`/`port` 一致且 `path()` 严格位于基路径分段边界内；启动时预校验 `upstream` 无 fragment/userinfo 且 `api_key` 为合法 `HeaderValue`。

### [P0-5] 跨 Space 审计日志越权泄漏（hark · W2-1）
- **位置**：[`auth/mod.rs:266-274`](../crates/om-server/src/auth/mod.rs#L266-L274)、[`db/audit.rs:68-101`](../crates/om-server/src/db/audit.rs#L68-L101)、[`db/migrations.rs:59-66`](../crates/om-server/src/db/migrations.rs#L59-L66)
- **根因**：`audit_logs` 无 `space_id` 字段，`Auth::audit_log` 全局查询，Space B 的 Admin/Owner 可读取 Space A 的全部审计记录。
- **修复建议**：迁移新增 `space_id TEXT` 与索引 `(space_id, id DESC)` / `(space_id, target, id DESC)`，`list_audit` 强制按 `p.space_id` 隔离。

### [P0-6] 审批记忆规则绕过路径范围、`.git` 保护与 `Direct` 模式（hark · W1-3）
- **位置**：[`approval/mod.rs:102-131`](../crates/om-server/src/approval/mod.rs#L102-L131)、[`166-181`](../crates/om-server/src/approval/mod.rs#L166-L181)
- **根因**：`rule_key()` 把 `ToolKind::Edit` 映射为裸字符串 `"edit"`，且 `evaluate()` 在校验 `SandboxMode::Bwrap` 与 `all_within(cwd, locations)`（含 `.git` 禁止自动放行）之前就应用 `remembered`。一次对编辑选“始终允许”后，后续对 `.git/*`、`$HOME` 或 `Direct` 模式宿主机任意文件的 `Edit` 全部自动放行。
- **修复建议**：禁止生成无路径通配的 `"edit"` 记忆键，且对文件类工具的记忆放行规则也必须强制执行 `.git` 排除与路径边界校验。

### [P0-7] `RpcConnection::request(..., None)` 在 Agent 退出时永久死锁（hark · W0-3）
- **位置**：[`acp/rpc.rs:125-148, 167-208`](../crates/om-server/src/acp/rpc.rs#L125-L208)、[`acp/harness.rs:202-205, 423-441`](../crates/om-server/src/acp/harness.rs#L202-L205)
- **根因**：reader 退出排空 `pending` 后未置关闭标记；若 `session/prompt`（无超时）随后插入 `pending` 且 writer 写 `EPIPE` 退出（不排空 `pending`），`rx.await` 永久挂起，导致 `handle_incoming` 的 `turn.await` 永久死锁。
- **修复建议**：将 `pending` 改为 `Mutex<Option<HashMap<...>>>`，reader/writer 退出时 `.take()` 并失败所有挂起请求，`request()` 见 `None` 立即返回 `CONNECTION_CLOSED`。

### [P0-8] ACP `session/update` 通知与 JSON-RPC 响应缺少因果保序屏障（hark · W0-3）
- **位置**：[`acp/rpc.rs:231-258`](../crates/om-server/src/acp/rpc.rs#L231-L258)、[`acp/harness.rs:115-185, 368-378, 423-441`](../crates/om-server/src/acp/harness.rs#L115-L185)
- **根因**：通知经 `in_tx` 异步消费而响应直连 `oneshot` 唤醒 `turn_task`/`handshake`，导致：
  1. `TurnEnded` 可抢先于尾部 `agent_message_chunk` 到达 `Actor`，被静默丢弃（甚至破坏 `[[goal:done]]` 判定）或误归属到队列下一回合；
  2. `session/load` 返回后立即清 `state.loading = false`，未排空的重放历史通知会漏入实时事件流。
- **修复建议**：在 `in_tx` 引入保序屏障（如 `Incoming::Fence(oneshot::Sender<()>)`）或由单任务串行派发响应结算。

### [P0-9] 前端静默丢弃 8 个新事件 + 服务端日志泄漏 WS 明文 Token（Antigravity · **本 PR 已修复**）
- **位置**：[`apps/web/src/protocol/reducer.ts`](../apps/web/src/protocol/reducer.ts)、[`crates/om-server/src/api/mod.rs`](../crates/om-server/src/api/mod.rs)
- **状态**：已在本 PR 修复（自定义 `TraceLayer` 仅记录 `uri().path()` 剥离 query；`reducer.ts` 补齐全部 21 个 `EventPayload` 变体 + `never` 穷尽检查 + `auto_rule`/`tool_kind`/`locations` 渲染）。

---

## 3. P1：高危并发竞态、隔离缺口与契约缺陷（16 项）

### hark 侧待修项（14 项）
1. **[P1-1] `BwrapSandbox` 在 `CgroupMode::SystemdUserScope` 下清空 `XDG_RUNTIME_DIR` 导致 `systemd-run --user` 必败**（[`sandbox/mod.rs:218-248, 385-402`](../crates/om-server/src/sandbox/mod.rs#L385-L402)）：应改用 `bwrap --clearenv --setenv K V` 隔离内层环境，外层保留 `systemd-run --user` 所需的 `XDG_RUNTIME_DIR`/`DBUS_SESSION_BUS_ADDRESS`。
2. **[P1-3] RBAC 读查/写改分离 TOCTOU 提权**（[`auth/mod.rs:153-263`](../crates/om-server/src/auth/mod.rs#L153-L263)、[`db/members.rs:225-433`](../crates/om-server/src/db/members.rs#L225-L433)）：将 actor 状态/角色与 target 角色校验下沉到同一 SQLite 写事务内原子执行。
3. **[P1-4] 审批策略 3 处绕过 / Fail-Open**：
   - [`actor.rs:430-462`](../crates/om-server/src/orchestrator/actor.rs#L430-L462)：成员选 `AllowAlways` 后把原始 `AllowAlways` 直接发给 Agent，真实 ACP Agent 内部缓存后 `clear_approval_rules` 无法撤销（应转译为 `AllowOnce` 发给 Agent，由服务端唯一托管记忆）；
   - [`approval/mod.rs:171-181`](../crates/om-server/src/approval/mod.rs#L171-L181) & [`acp/harness.rs:143-151`](../crates/om-server/src/acp/harness.rs#L143-L151)：空 `title` 或兜底 `"Permission requested"` 产生通配键 `"execute:"` / `"execute:Permission requested"`；
   - [`approval/mod.rs:204-229`](../crates/om-server/src/approval/mod.rs#L204-L229) & [`actor.rs:550-562`](../crates/om-server/src/orchestrator/actor.rs#L550-L562)：`all_within` 接受 `""`；`load_rules()` 遇 DB 错误保留旧缓存导致 fail-open。
4. **[P1-5] `WorkspaceManager::read_file` TOCTOU 符号链接逃逸 & `diff()` 无界 OOM / FIFO 卡死**（[`workspace/mod.rs:503-555, 597-636`](../crates/om-server/src/workspace/mod.rs#L503-L636)）：`read_file` 需改为 `openat(..., O_NOFOLLOW | O_NONBLOCK)` + `fstat` + 限长读；`diff` 需流式限长读 stdout、跳过非普通文件（防 FIFO 卡满 120s）、修复 `patch.len() == max_patch_bytes` 时 `truncated = false`。
5. **[P1-6] Worktree 生命周期锁缺失、目录泄漏与跨 Space 重复注册**（[`workspace/mod.rs:164-228, 396-416, 639-662`](../crates/om-server/src/workspace/mod.rs#L396-L416)、[`db/migrations.rs:83`](../crates/om-server/src/db/migrations.rs#L83)）：`discard()` 缺 `repo_locks`；删 Workspace 后 `ON DELETE SET NULL` 导致 `remove_worktree` 跳过清理磁盘；恶意 Agent 删 `.git` 可阻断清理；`register()` 需 canonicalize 并校验 `git_common_dir` 重叠及跨 Space 唯一性，按 `git_common_dir` 建锁。
6. **[P1-7] `KeyProxy` 宿主内存/连接 DoS 与 `by_session` 泄漏**（[`proxy/mod.rs:262-270, 330-336, 528-541`](../crates/om-server/src/proxy/mod.rs#L528-L541)）：需加全局/单会话并发上限、body 读超时与 `reqwest` `read_timeout`，TTL 过期时同步清理 `by_session`。
7. **[P1-8] `resume_session` / `spawn_actor` / `close_session` 并发 TOCTOU 覆盖并误吊销 `KeyProxy` Token**（[`orchestrator/mod.rs:231-259, 348-374, 512-529`](../crates/om-server/src/orchestrator/mod.rs#L348-L374)）：建议带代次号原子占位 + RAII `Drop` 清理守卫。
8. **[P1-9] Queue & Steer 状态机 3 处缺陷**（[`actor.rs:155-171, 294-310, 370-385`](../crates/om-server/src/orchestrator/actor.rs#L294-L310)、[`db/mod.rs:580-639`](../crates/om-server/src/db/mod.rs#L580-L639)）：`Idle`+暂停队列下 `on_send` 误报 `SubmitOutcome::Queued` 及先清 `paused` 后失败；`on_withdraw` 清空队列/`on_prompt` 开新回合未清 `paused`；`recover_after_restart()` 未补发 `TurnEnded { Failed }`、`QueueItemRemoved { Dropped }` 与 `QueuePaused { paused: false }`。
9. **[P1-10] `EventHub` 内存泄漏、`last_seq > head_seq` 吞事件、`announce` 漏广播与 `acp/rpc.rs` 无界读行**（[`hub.rs:20-49, 69-80`](../crates/om-server/src/orchestrator/hub.rs#L20-L49)、[`orchestrator/mod.rs:414-439`](../crates/om-server/src/orchestrator/mod.rs#L414-L439)、[`acp/rpc.rs:126`](../crates/om-server/src/acp/rpc.rs#L126)）。
10. **[P1-11] `ScheduleSpec::Every` 负 `start_at` 溢出导致 Panic 或 100% CPU 无限触发**（[`om-core/src/schedule.rs:104-111, 128-138`](../crates/om-core/src/schedule.rs#L128-L138)）。
11. **[P1-12] `Scheduler` 的 `OverlapPolicy::Skip` 120s TOCTOU、失败未关子会话导致永久卡死、无并发限流**（[`scheduler/mod.rs:204-236, 383-426`](../crates/om-server/src/scheduler/mod.rs#L383-L426)）。
12. **[P1-13] `GoalRunner` `set_goal`/`after_turn`/`stop_goal` 竞态、未按 `turn_id`/`queued_id` 关联导致多跑/卡死、缺 `deadline_at` 定时器**（[`scheduler/goals.rs:80, 102-171, 285-410`](../crates/om-server/src/scheduler/goals.rs#L102-L171)）。
13. **[P1-14] `Dispatcher` `send_prompt` 先于 `insert_dispatch` 绕过深度限制、无扇出宽度限制、深度重置/永久锁死、失败未关子会话、未校验运行中及忽略 `prompt_steered`**（[`scheduler/dispatch.rs:60-143`](../crates/om-server/src/scheduler/dispatch.rs#L60-L143)、[`db/goals.rs:188-222`](../crates/om-server/src/db/goals.rs#L188-L222)）。
14. **[P1-15] `om-core/src/auth.rs` 6 个时间戳缺 `#[ts(type = "number")]` 导出为 JS `bigint`**（[`om-core/src/auth.rs:96-126`](../crates/om-core/src/auth.rs#L96-L126)），且 `om-core/src/api.rs` 需补充 W1/W2/W4 REST DTO（`SubmitPromptRequest` 增 `mode?: SubmitMode`、`SubmitPromptResponse`/`SubmitOutcome`、`AuditEntry`、`ApprovalRule`、`HarnessDescriptor`、`ProxyStats` 及 workspaces/auth/queue/schedules/goals/dispatches 请求响应体）。

### Antigravity 侧修复项（本 PR 已修）
- **[P1-2]** [`harness/opencode.rs`](../crates/om-server/src/harness/opencode.rs)：启用 `clear_env = true` 并仅透传安全基础变量（杜绝 `DirectSandbox` 泄漏 `OM_TOKEN`），新增 `OpenCodeConfig::from_env_for_proxy()` 供 `KeyProxy`/`BwrapSandbox` 启用时彻底剥离宿主明文 API Key。
- **[P1-10 WS]** [`api/ws.rs`](../crates/om-server/src/api/ws.rs)：将下行队列改为有界通道，慢客户端队列满时发送 `ServerMessage::Resync` 并优雅终止该订阅任务，避免半开连接致服务端 OOM。
- **[P1-16 前端]** [`apps/web/`](../apps/web)：`reducer.ts` 严格以 `status_changed` 为唯一权威状态源、`NewSessionModal` 动态拉取 `GET /api/v1/harnesses`、新增关闭会话与 WS `Ping` 心跳保活、修复致命 WS 错误无限重连。

---

## 4. P2：健壮性与工程改进（供后续顺手清理）

- **hark 侧**：
  1. [`sandbox/mod.rs:177-203, 307-311, 363-379`](../crates/om-server/src/sandbox/mod.rs#L363-L379)：`is_covered` 需词法归一化；binary 挂载解析 `canonicalize` 符号链接（修复 npm 全局 `opencode`）且勿整目录挂载 `program.parent()`；`--tmpfs` 设 `--size`；[`tests/sandbox_bwrap.rs:126`](../crates/om-server/tests/sandbox_bwrap.rs#L126) 兼容内核 `sit0` 接口。
  2. [`workspace/mod.rs:247-255`](../crates/om-server/src/workspace/mod.rs#L247-L255)：按 `space_id` 隔离成员 `$HOME` 并消除 `MemberId("_anonymous")` 碰撞。
  3. [`auth/mod.rs:206-236`](../crates/om-server/src/auth/mod.rs#L206-L236)：拒绝 `< 1ms` TTL、限制单成员 Token 数量、禁止短命 Token 自签永久 Token。
  4. [`proxy/mod.rs:413-448, 506`](../crates/om-server/src/proxy/mod.rs#L413-L448)：剥离 `Connection` 点名的 hop-by-hop headers、限制 HTTP 方法白名单。
  5. [`actor.rs:206, 512-548, 821-830`](../crates/om-server/src/orchestrator/actor.rs#L512-L548)：持久化 `HarnessEvent::NativeSessionId`（Phase 1 `cc`/`codexcli`/`dsh` 依赖）；`expire_due` 瞬时 DB 错误重试；`start_harness` 支持 `Close` 打断。
  6. [`db/mod.rs`](../crates/om-server/src/db/mod.rs) / [`db/audit.rs`](../crates/om-server/src/db/audit.rs) / [`db/workspaces.rs`](../crates/om-server/src/db/workspaces.rs)：`list_audit` 拆查询走索引、`migrate()` 拒绝未来版本、写事务改用 `TransactionBehavior::Immediate`、`list_events` 用只读事务防快照偏斜且检查 `after_seq` 溢出、`list_approvals` 校验会话存在、区分 `SQLITE_CONSTRAINT_UNIQUE` 与 `FOREIGNKEY`。
  7. [`scheduler/mod.rs:123-200`](../crates/om-server/src/scheduler/mod.rs#L123-L200) & [`scheduler/dispatch.rs:75-79`](../crates/om-server/src/scheduler/dispatch.rs#L75-L79)：`set_enabled` 局部 UPDATE 避免覆盖并发 `update()`、统一跨 Space 返回 `NotFound`、`set_enabled`/`delete` 增加 `by: &MemberId`。
  8. [`om-core/src/api.rs:132-154`](../crates/om-core/src/api.rs#L132-L154) & [`schedule.rs`](../crates/om-core/src/schedule.rs)：`Internal` 类错误对外脱敏、`LimitExceeded` 可映射 `429`、`#[serde(default)]` 字段补 `#[ts(optional)]`。
- **Antigravity 侧（本 PR 已修）**：`api/mod.rs` CORS 补 `PATCH`/`PUT`；`api/auth.rs` 使用 `subtle` + SHA-256 恒定时间对比；`httpClient.ts` 读取 `/healthz` 真实版本并避免登录重复请求 `listSessions()`。

---

## 5. 后端核心 vs API / Web / CLI 接入矩阵

| 模块 | `om-core` 契约 | `om-server` 核心库 | `main.rs` 装配 | `api/` 路由 | `apps/web/` | `om-cli` |
| --- | --- | --- | --- | --- | --- | --- |
| **W0 核心会话/事件/审批** | 就绪 | 就绪 | 已接入 | 已接入 | 已接入 | 占位 |
| **W1-1 `bwrap` 沙箱** | 就绪 | 就绪（待修 P1-1） | 待接入 | N/A | 待接入 | 待接入 |
| **W1-2 Git Worktree** | 缺 REST DTO | 就绪（待修 **P0-1**） | 待接入 | 待接入 | 待接入 | 待接入 |
| **W1-3 审批策略与审计** | 缺 DTO | 就绪（待修 **P0-6**） | 待配置 | 待接入 | 已补投影/渲染 | 待接入 |
| **W1-4 多 CLI (`dsh`/`cc`/`codexcli`)** | 就绪 | 待实现 | 待接入 | 已就绪 | 已改动态拉取 | 待接入 |
| **W2-1 30人 RBAC** | 待修 `bigint` + 缺 DTO | 就绪（待修 **P0-5**） | 待接入 | 待接入 | 待接入 | 待接入 |
| **W2-2 宿主机 Key 代理** | 缺 DTO | 就绪（待修 **P0-3/P0-4**） | 待接入 | 待接入 | 待接入 | 待接入 |
| **W2-3 Queue & Steer** | 缺 `mode`/响应 DTO | 就绪（待修 P1-9） | 编排就绪 | 待接入 | 已补事件投影 | 待接入 |
| **W4-1 Schedules/Goals/Dispatch** | 缺 DTO | 就绪（待修 **P0-2**） | 待接入 | 待接入 | 已补事件投影 | 待接入 |

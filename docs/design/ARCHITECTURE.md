# subswap 架构设计

## 1. 分层

```
┌──────────────────────────────────────────────────────────────┐
│ CLI / Daemon 表面层                                            │
│  - crates/cli      `subswap`  (clap, 同步调用 Provider)        │
│  - crates/daemon   `subswapd` (周期采样 + 自动切换, M4)        │
└───────────────────────────────┬──────────────────────────────┘
                                │
┌───────────────────────────────▼──────────────────────────────┐
│ 用例层（在 cli/daemon 内联，简单足够，不抽独立 crate）         │
│  - AutoSwapPolicy（core 纯函数）/ AuditLog（core）             │
└───────────────────────────────┬──────────────────────────────┘
                                │
┌───────────────────────────────▼──────────────────────────────┐
│ Provider 抽象 (crates/core)                                    │
│  - trait Provider                                              │
│  - struct ProviderRegistry                                     │
│  - struct Account / Quota / ClientTarget                       │
└──────────┬─────────────────────────────────────┬─────────────┘
           │                                     │
┌──────────▼────────────┐             ┌──────────▼─────────────┐
│ providers/codex       │             │ providers/claude        │
│ providers/kimi        │             │ - keyring + 备份替换    │
│ providers/opencode    │             │ - Anthropic usage 端点  │
│ - Codex/Kimi/OpenCode │             │ - 同步 ~/.claude        │
│   Runtime             │             │ - 自定义 API 账号       │
│ - 只写差异点：本地路径│             │ - 独立实现，不接 common │
│   解析/元数据/刷新/   │             │                         │
│   usage 查询          │             │                         │
└──────────┬────────────┘             └──────────┬─────────────┘
           │ 实现 FileBlobRuntime                 │
┌──────────▼────────────┐                         │
│ providers/common       │                        │
│（文件型 OAuth 切换共享引擎）                     │
│ - FileBlobProvider<A>： │                        │
│   activate/query_quota/│                         │
│   capture-on-leave/    │                         │
│   隔离导出导入（机制） │                         │
│ - IsolatedProvider：    │                        │
│   run/shell/env 隔离   │                         │
│   运行的 blanket impl  │                         │
└──────────┬─────────────┘                         │
           │                                       │
┌──────────▼───────────────────────────────────────▼─────────────┐
│ providers/cursor（独立 Provider）                               │
│ - SQLite state.vscdb + GUI 退出/重启协调                        │
│ - 事务切换 / 回滚 / Cursor usage-summary                        │
│ - 不接 common，不支持 run/shell/env 隔离                        │
└───────────────────────────────┬────────────────────────────────┘
                                │
┌──────────▼───────────────────────────────────────▼─────────────┐
│ 平台抽象 (crates/core)                                          │
│  - CredentialStore (trait) → FileStore（默认）/ KeyringStore（迁移）│
│  - AppPaths (XDG / Library / AppData)                           │
└──────────────────────────────────────────────────────────────────┘
```

Codex / Kimi / OpenCode Go → `providers/common`（原子写、快照回滚、capture-on-leave、parked-only 刷新、隔离导出/吸收）；各自只实现 `FileBlobRuntime` 差异点。OpenCode 另覆盖 `extract_blob` / `compose_live`（`auth.json` 多供应商共存，只改 `opencode-go` 项）。Claude（Keychain + 自定义 API、无本地凭证文件）与 Cursor（SQLite + GUI 生命周期）独立实现 `Provider`，不接共享引擎。

## 2. 设计模式

| 模式 | 落地位置 | 作用 |
|---|---|---|
| Strategy + Factory | `Provider` trait + `ProviderRegistry` | 多 Provider 多策略，新增 = 加一行注册 |
| Adapter | `providers/codex`、`providers/kimi`、`providers/opencode` | 各自实现 `FileBlobRuntime`，把本地路径/元数据/刷新/usage 查询的差异适配进 `providers/common` 共享引擎；Claude/Cursor 直接实现 `Provider` trait，不属于这个 Adapter 关系 |
| Repository | `CredentialStore` trait + `FileStore` / `KeyringStore` | 默认私有文件仓库；旧 keyring 只作懒迁移源 |
| Observer | M4 的 `UsageMonitor` → `AutoSwapPolicy` | 周期采样触发自动切换 |
| Chain of Responsibility | M4 的 `AutoSwapPolicy` 内部 | 阈值 → 限流 → 候选筛选 → 选优 |

## 3. 关键数据流

### 3.1 `subswap`（无参默认入口）

```
① sync_local_active
   └─ claude/codex/kimi/cursor 同步当前本地账号
      （读各原生客户端登录状态，upsert registry；失败静默跳过）

② build_loading_snapshots
   └─ 只读 registry，立即渲染账号骨架；quota 显示 loading

③ fill_quotas_progressively（并发）
   ├─ N 个 query_quota 并发；每个账号返回后刷新对应行
   ├─ 单个 Provider 的账号全部返回后，立即对该 Provider 跑 AutoSwapPolicy
   └─ 如需切换：Provider.activate(to) → write audit → 标记当前快照 active
      （交互终端渐进刷新；非交互/管道只输出最终状态）

④ auto_decide（纯函数，无 IO）
   └─ AutoSwapPolicy：usage_ratio >= defaults::AUTO_SWAP_THRESHOLD → Swap
                     active quota 查询失败 → Degraded（提示手动 swap）
                     否则 → NoOp

⑤ render 最终状态
```

`find_unique(id)`：全局 id 反查（唯一可省略 provider；歧义用 `<provider>/<id>`）。
默认入口全局编号（跨 provider 连续，1-based）来自 `AppContext::list_ordered()`，与 `subswap swap N` / `subswap rm N` 同映射。
tty：ANSI 分层（active `*` bold cyan、warn 黄、full 加粗红；其余 dim）；非交互退化为纯文本。

交互要求：
- 账号列表必须先于全部网络请求出现。
- quota 行统一余量块（如 `5h [ 41% left ]`、`1st [ 41% left ]`）；无有效数据的窗口不显示（如 Claude `extra_usage` 缺 utilization 时不输出 `mo=?`）。
- reset 默认相对时间（`in 69m` / `in 4h` / `in 3d`）。

### 3.2 `subswap login <provider>`

```
claude: subswap login claude → claude auth login --claudeai → claude.import_active()
codex:  subswap login codex  → codex login                 → codex.import_active()
kimi:     subswap login kimi     → （用户自己先跑 kimi 登录）     → kimi.import_active()
cursor:   subswap login cursor   → （用户自己先在 Cursor 登录）   → cursor.import_active()
opencode: subswap login opencode → （TUI 已 connect，或 `-- sk-…`）→ import_active / import_raw
                                      └─ registry.set_active(provider, imported_id)
```

取舍：
- 不复刻私有 OAuth；优先委托厂商官方 CLI。
- 同账号重 login 按 `(provider, id)` 覆盖凭证，不新增重复。
- 完成后以官方 CLI 当前激活账号为准，导入并标 active。
- Kimi / Cursor / OpenCode：无驱动登录子命令 → 只 import（用户先自行登录；OpenCode 亦可 `login opencode -- sk-...` 写 API key）。切换 OpenCode 只改 `opencode-go` 项，不得覆盖同文件其它供应商。

### 3.3 `subswap swap [<id|N>]`

```
resolve_account(input):
   ├─ 纯数字 N → list_ordered()[N-1]
   └─ 否则     → find_unique(input)
Provider.activate(id)
   ├─ 按 Provider 安全边界做 best-effort 凭证恢复
   ├─ 文件型：flock → snapshot → 原子写文件 → 写 registry
   ├─ Cursor：正常退出 GUI → capture → SQLite transaction → 写 registry → 重启确认
   └─ 写 audit
```

无参 `subswap swap`：只打印 `Usage: ...` + 带编号清单（不查 quota；手动入口零网络依赖不变量）。切到具体账号成功后再打余量表（§3.3.1），**切换本身仍不依赖 quota**。

**重要**：此路径不依赖 `query_quota`，网络不通仍可用。`subswap rm` 同用 `resolve_account`。

### 3.3.1 写操作后的状态面（status-after-action）

`add-api` / `login` / `swap <目标>` / `rm` 成功后 → `print_status_overview`：只读 registry → 渐进拉 quota → `render_to_string`。

```
写操作成功
   └─ 一行结果（added / login / swap / removed）
      └─ print_status_overview
         ├─ build_loading_snapshots（当前 registry，不再 sync 本地登录）
         ├─ fill_quotas_progressively(enable_auto_swap=false)
         └─ render 最终状态（不拉起 daemon）
```

禁止收尾再调 `default::run`：会 sync 本地登录（Cursor/`rm` 后仍登录号被导回）、AutoSwap（顶掉刚手动切的号）、拉起 daemon。

### 3.3.5 Claude 自定义 API

```
subswap add-api
   ├─ 交互向导 / DeepSeek 预设
   ├─ API Key → CredentialStore(field=api_key)
   └─ 非敏感端点与模型映射 → registry extra(kind=api, manual_only=true)

subswap swap <api-id>
   ├─ 捕获切入前 settings.json.env 受管字段
   ├─ 合并写入 API endpoint / key / 模型映射
   └─ 写 .subswap-api.json 激活标记

subswap swap <oauth-id>
   ├─ 正常恢复 OAuth credentials + oauthAccount
   ├─ 恢复进入 API 模式前的 settings.json.env 受管字段
   └─ 删除 .subswap-api.json
```

属 `claude` Provider（列表/编号/`swap`/`rm` 一致）；无 quota；`manual_only` 禁止自动切入/切出。

### 3.4 `subswapd` daemon（M4）

```
每 defaults::DAEMON_POLL_INTERVAL_MS（默认 60s）：
   ├─ capture-on-arrival（Codex/Kimi/Cursor live→store）
   ├─ build_snapshots → auto_decide → 重验 active 未变且非 manual_only → activate（如需）
   ├─ 对非活跃 Claude 账号：若 expires_at < now + REFRESH_SLACK_MS → refresh_account
   └─ 写 audit
```

降级路径见 [AUTO_SWAP_DESIGN.md](AUTO_SWAP_DESIGN.md#降级到手动)。

## 4. 凭证与文件布局

### 4.1 凭证仓库（敏感）

```
key:   {provider}:{account}:{field}
field 例： credentials_json（Claude 整段）/ auth_json（Codex 整段）
```

`crates/core/src/store.rs::CredentialStore` + `compose_key()` → `{provider}:{account}:{field}`。读不存在 → `Ok(None)`；仅平台/IO 错误 → `Err`。

- **`FileStore`（默认）**：`<data_dir>/credentials.json`，Unix `0600`。`AppContext::build()` / daemon `run()` 默认装配。可 `with_legacy_keyring`：文件未命中时从 `KeyringStore` 读出落盘（按需一次性迁移，之后不再碰钥匙串）。
- **`KeyringStore`**：仅作迁移回退源。

默认走文件：macOS 钥匙串每次读写可能弹授权，重编译换身份会反复弹框（见 troubleshooting `2026-05-29-macos-keychain-prompts` / `2026-06-06-filestore-credential-backend`）。代价：token 明文落盘（`0600`，与 `~/.codex/auth.json` 同级）。

**`KeyringStore` 多端后端（迁移回退）**：

| 平台 | keyring 后端 | 进程间可见 | 重启后持久 |
|---|---|---|---|
| macOS | Keychain | ✅ | ✅ |
| Windows | Credential Manager | ✅ | ✅ |
| Linux | `linux-keyutils`（内核 keyring，编译期默认 feature） | ⚠️ 按内核 session 隔离 | ❌ 默认不跨重启 |

Linux keyutils 按**内核 session keyring** 隔离。`subswapd` 经 `fork + setsid`（`crates/cli/src/daemon_spawn.rs`）进新 session → daemon 读不到 CLI session 写入项。**`FileStore` 后此隔离消失**（见 troubleshooting `2026-05-29-daemon-keyutils-session-isolation`）。推论：token 自愈仍不只依赖 daemon；查询/切换路径也能 best-effort 刷新。

### 4.2 subswap 应用目录

平台路径仅为未覆盖时的默认**配置目录**；`SUBSWAP_HOME` 统一覆盖、精确映射及 Cursor 原生库不随之迁移的边界 → [CONFIG.md](../CONFIG.md)「应用目录覆盖（高级）」唯一来源。

| 平台 | 路径 |
|---|---|
| Linux | `$XDG_CONFIG_HOME/subswap/` 或 `~/.config/subswap/` |
| macOS | `~/Library/Application Support/dev.subswap.subswap/` |
| Windows | `%APPDATA%\subswap\subswap\config\` |

- 配置目录：`config.toml`、`registry.toml` 等明文配置与账号元数据。
- 数据目录：凭证仓库、切换审计、daemon 日志、隔离运行目录。
- 状态目录：切换快照、daemon PID、Provider 跨进程协调（实现在数据目录 `state/`）。
- 缓存目录：共享额度查询缓存。

### 4.3 Provider 私有目录（沿用上游）

- Codex：`~/.codex/accounts/registry.json` + `~/.codex/sessions/`
- Claude：`~/.claude/`
- Kimi：`~/.kimi-code/credentials/kimi-code.json`（`KIMI_CODE_HOME` 可覆盖）
- OpenCode Go：`~/.local/share/opencode/auth.json` 的 `opencode-go` 项（`SUBSWAP_OPENCODE_HOME` 可覆盖）
- Cursor：各平台 `Cursor/User/globalStorage/state.vscdb`（详见 Provider 知识库）

切换写上游状态；完整 token 在 `FileStore`；`registry.toml` 只存非敏感元数据。

## 5. 扩展新 Provider 的步骤

**文件型 JSON 凭证、切换=原子覆盖**（Codex/Kimi 同款）→ 复用共享引擎：

1. 新建 `crates/providers/<id>/`，依赖 `subswap-core` + `subswap-provider-common`。
2. 实现 `FileBlobRuntime`（`crates/providers/common/src/runtime.rs`）；机制由 `FileBlobProvider<A>` 提供。
3. `AppContext::build()` 注册一行；若支持 `run/shell/env`，插入 `isolated: HashMap<&str, Arc<dyn IsolatedProvider>>`（`FileBlobRuntime` 有隔离能力时自动获 blanket impl，见 `isolated.rs`）——之后 `run.rs` 分发查表即可。
4. `run.rs::normalize_provider` 加别名匹配（纯文本解析，查表吸收不了）。
5. `login.rs` 加专属 `match` 分支（登录从未通用查表：Codex/`codex login`、Claude/`claude auth login --claudeai`、Kimi 纯导入，语义各异）。
6. `crates/cli/Cargo.toml` 加依赖；`sync_local_active()` 加 `import_active`。
7. `docs/PROVIDER_KNOWLEDGE_BASE.md` 补接口/坑（含 adapter 差异点表）。

**形状不同**（Claude Keychain / Cursor SQLite+GUI）→ 不接共享引擎：

1. 新建 crate，依赖 `subswap-core`。
2. 实现 `Provider`（`list_accounts / activate / query_quota / client_targets`）。
3. `AppContext::build()` 注册；Cargo.toml 依赖；`sync_local_active()` 加 import。
4. `PROVIDER_KNOWLEDGE_BASE.md` 补笔记。

`run/shell/env` 取决于凭证能否安全投影到独立目录。Cursor SQLite+GUI 不满足 → 禁止注册 `AppContext.isolated`。

不要在 `core` 写任何 Provider 特定逻辑。

## 5.5 数值调优常量的管理

**运行期** → `crates/core/src/settings.rs::current()`（`<config_dir>/config.toml`，热生效）；
**编译期默认** → `crates/core/src/defaults.rs`（`Settings::default()` 从此读）。
provider / cli / daemon 禁止硬编码阈值、时间窗口、百分比。

| 字段路径 | 默认值 | 说明 |
|---|---|---|
| `auto_swap.threshold` | `defaults::AUTO_SWAP_THRESHOLD` | AutoSwap 触发阈值（0.0~1.0） |
| `auto_swap.cooldown_ms` | `300_000` ms | 切换后单账号冷却期（daemon） |
| `quota.warn_pct` | `90.0` | Quota 视觉 Warn 阈值（百分比） |
| `quota.exhausted_pct` | `100.0` | Quota Exhausted 阈值（百分比） |
| `token.refresh_slack_ms` | `300_000` ms | token 预刷新提前量（5 min） |
| `daemon.poll_interval_ms` | `60_000` ms | daemon 活跃时轮询周期 |
| `daemon.idle_threshold_ms` | `1_800_000` ms | probe mtime 距今超过此值 → 空闲 |
| `daemon.idle_poll_interval_ms` | `900_000` ms | daemon 空闲时轮询周期 |
| `codex.usage_cache_max_age_ms` | `600_000` ms | 旧版 Codex 本地 last_usage 缓存最大年龄 |

调字段：改 `config.toml`；改默认改 `defaults.rs` + AGENTS.md 不变量 #4。完整说明 → [CONFIG.md](../CONFIG.md)。

### Daemon 空闲退避

主循环每轮：
1. `settings::reload_from_file()`。
2. 扫各 provider `client_targets().probe_path` mtime；距今 ≥ `idle_threshold_ms` → `idle_poll_interval_ms`，否则 `poll_interval_ms`。
3. probe 不存在 / 无 mtime → 按空闲（保守，避免凭空高频轮询）。

长时间不用 → 放慢；官方 CLI 调 API 写回 token → 回到活跃节奏。

## 5.6 风控边界

自动切换不能靠高频请求探测额度或制造 429。CLI 无参入口仅用户主动时采样一次；daemon 按 `DAEMON_POLL_INTERVAL_MS` 低频轮询，失败退避。未来 429 立即切换只能来自真实客户端 hook / 本地 IPC，不能靠更密 usage 请求。

CLI 与 daemon 共用持久 `quota_cache.json`。新鲜度 < `settings.quota.min_refresh_interval_ms`（默认 90 秒）复用；之外拉实时，失败可带时间戳显示 stale。限制请求频率，不把旧结果伪装成实时。

## 6. 错误处理

- `core::error::Error` 统一枚举。Provider 内部 `anyhow::Error` → `Error::Other` / `Error::Provider(String)`。
- CLI：`anyhow::Result` + `with_context`。
- `query_quota` 失败返回 `Err`，不静默吞；CLI 决定是否降级。

## 7. 关键代码路径地图

> 改动下列流程时同步更新本表。只记函数名（比行号稳定）。

### 7.1 凭证存储（keyring）

| 职责 | 位置 |
|---|---|
| `CredentialStore` trait + `KeyringStore` 实现 + `compose_key` | `crates/core/src/store.rs` |
| 多端后端差异 / keyutils session 隔离坑 | 本文 §4.1 + troubleshooting/2026-05-29-daemon-keyutils-session-isolation.md |

### 7.2 调优参数（settings / defaults）

| 职责 | 位置 |
|---|---|
| 编译期默认常量 | `crates/core/src/defaults.rs` |
| 运行期值 `current()` / 热加载 `reload_from_file()` / `load_from()` / `Settings` 分组 | `crates/core/src/settings.rs` |
| 字段表 / 风控约束 | `docs/CONFIG.md` |

### 7.3 Claude provider（`crates/providers/claude/src/`）

| 职责 | 函数 / 文件 |
|---|---|
| 拉 quota（401 时进程内 best-effort 刷新并重试一次） | `lib.rs::ClaudeProvider::query_quota` |
| 手动切换（阶段1 best-effort 预刷新，失败只 warn 不阻塞） | `lib.rs::activate` + `lib.rs::best_effort_pre_refresh` |
| daemon 保活：仅临近过期才刷 | `lib.rs::refresh_if_near_expiry` |
| 显式无条件刷新 | `lib.rs::refresh_account` |
| 纯刷新逻辑（不碰 keyring/磁盘，调用方负责持久化） | `lib.rs::apply_refresh_to_creds` |
| 过期判断（看 `expiresAt` + `refresh_slack_ms`） | `lib.rs::is_expired_or_soon` |
| 401 判定 | `lib.rs::is_auth_error` |
| keyring 读写本账号凭证（field=credentials） | `lib.rs::load_credentials` / `save_credentials` |
| 入库（keyring + registry，复用 active 标记） | `lib.rs::store_account` |
| usage → `Quota` + 视觉状态 | `lib.rs::make_quota` |
| 上游端点：`fetch_usage`(GET usage) / `refresh_access_token`(POST oauth/token) | `oauth.rs` |
| `~/.claude/.credentials.json` schema（camelCase） | `claude_files.rs` |
| credentials_path / global_config_path | `paths.rs` |
| 自定义 API 登记 / 切入 / OAuth 恢复 | `lib.rs::add_api` / `activate_api` / `activate` |
| `settings.json` API env 合并与恢复 | `claude_files.rs` |

> 401 在 `oauth::fetch_usage` 里变成 `Error::QuotaFetch("usage returned 401 ...")`；`query_quota` 靠
> `is_auth_error` substring 判它再决定是否刷新。端点常量与各状态码真实含义见
> [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)。

### 7.4 Codex provider（`crates/providers/codex/src/`）

自 Task 8a/8b 起 Codex 跑在共享引擎（§7.5）上，`runtime.rs` 只是纯转发的 adapter：

| 职责 | 函数 / 文件 |
|---|---|
| `FileBlobRuntime` adapter（纯转发，不新增逻辑） | `runtime.rs::CodexRuntime` |
| 差异点：`store_field()→"auth_json"` / `dedup_extra_key()→"chatgpt_account_id"`（迁移前存量数据兼容） | `runtime.rs` |
| legacy 恢复（store/live 都拿不到时从 `~/.codex/accounts/` 找回）+ 隔离物化时拷 `config.toml` | `legacy.rs::recover_legacy_auth_for_account` / `copy_codex_config_best_effort` |
| active 官方额度查询（control socket / 临时 app-server / 安全刷新一次） | `app_server.rs::fetch_usage` / `AppServerSession::query_rate_limits` |
| parked 兼容查询 + active 官方通道 fallback + legacy 缓存回退 | `quota.rs::fetch_codex_quota` |
| usage 解析（字段不稳定，容错） | `openai_usage.rs` |
| `~/.codex/auth.json` opaque 透传 schema | `codex_files.rs` |
| 路径 | `paths.rs` |

> `runtime.rs::CodexRuntime::refresh` 仍返回 `Unsupported`，所以共享引擎不会自行刷新 parked 账号；
> active 的唯一刷新入口是官方 app-server，设计边界见 Provider 知识库「Codex 官方额度通道」。

### 7.5 文件型 OAuth 切换共享引擎（`crates/providers/common/src/`）

| 职责 | 函数 / 文件 |
|---|---|
| adapter 契约（每个 runtime 的差异点，含 `store_field()`/`dedup_extra_key()` 两个兼容 hook） | `runtime.rs::FileBlobRuntime` |
| 机制实现：原子切换 / capture-on-leave / capture-on-arrival / parked-only 刷新 / 取 blob fallback 链 | `engine.rs::FileBlobProvider<A>` |
| 隔离运行的对象安全抽象（供 `run.rs` 查表分发，不必按 provider 硬编码分支） | `isolated.rs::IsolatedProvider`（`FileBlobRuntime` 的 blanket impl） |

完整职责边界与 adapter 差异点表见
[PROVIDER_KNOWLEDGE_BASE.md「文件型 OAuth 切换共享引擎」](../PROVIDER_KNOWLEDGE_BASE.md#文件型-oauth-切换共享引擎crates-providers-common)。

### 7.6 Kimi provider（`crates/providers/kimi/src/`）

| 职责 | 函数 / 文件 |
|---|---|
| `FileBlobRuntime` adapter（组装成 `KimiProvider = FileBlobProvider<KimiRuntime>`） | `lib.rs::KimiRuntime` |
| 路径解析（`KIMI_CODE_HOME` 环境变量覆盖） | `paths.rs` |
| JWT access_token 解析元数据（`user_id`/`client_id`/`scope`，无 email） | `kimi_files.rs::parse_metadata` / `decode_jwt_payload` |
| parked OAuth 刷新（`KIMI_CODE_OAUTH_HOST` 覆盖） | `oauth.rs::refresh_blob` |
| active 401：识别 Python 文件锁 / TypeScript proper-lock 目录锁，锁内恢复一次 | `oauth.rs::recover_active_401` / `recover_active_401_at` |
| usage 查询、active 安全恢复与窗口映射（`KIMI_CODE_BASE_URL` 覆盖） | `kimi_usage.rs::fetch_quota_with_active_recovery` / `parse_usages` |

端点、令牌生命周期、窗口映射细节见 PROVIDER_KNOWLEDGE_BASE.md「Kimi / Moonshot」一节。

### 7.6.5 Cursor provider（`crates/providers/cursor/src/`）

| 职责 | 函数 / 文件 |
|---|---|
| 跨平台 `state.vscdb` 路径、live blob 读取/事务写入 | `lib.rs::default_state_db_path` / `read_live_blob` / `write_blob_to_transaction` |
| 导入当前桌面端账号 | `lib.rs::CursorProvider::import_active` |
| GUI 正常退出 → capture → SQLite 事务切换 → 重启确认；失败回滚 | `lib.rs::CursorProvider::activate_blocking` / `SystemCursorProcessControl` |
| usage cookie 请求与 First-Party Models / API 窗口解析 | `lib.rs::CursorProvider::fetch_usage` / `parse_usage` |
| active 401 只重读 live；parked 跨进程锁内刷新与 dead guard | `lib.rs::CursorProvider::query_quota_inner` / `refresh_parked` / `RefreshLock` |

Cursor 不接 `FileBlobProvider`，也不注册 `IsolatedProvider`。完整安全边界见 Provider 知识库「Cursor」。

### 7.7 daemon（`crates/daemon/src/`，Unix-only）

| 职责 | 位置 |
|---|---|
| 主循环 + 空闲退避选周期 | `unix.rs::decide_next_interval` 及主循环 |
| 每账号 `query_quota` 收快照（失败记 `QuotaFetchState::Failed`） | `unix.rs`（snapshot 收集） |
| Claude token 后台保活（遍历所有账号调 `refresh_if_near_expiry`） | `unix.rs::keep_claude_tokens_alive` |
| 单实例 PID 文件锁 | `unix.rs::open_pid_lock` / `write_pid` |
| CLI 无感拉起（`fork + setsid` + stdio 重定向到日志） | `crates/cli/src/daemon_spawn.rs::ensure_daemon_running` / `spawn_detached_daemon` |

### 7.8 CLI（`crates/cli/src/`）

| 职责 | 位置 |
|---|---|
| `AppContext`（注册所有 provider + `isolated: HashMap<&str, Arc<dyn IsolatedProvider>>` 隔离分发表，**定义在 app.rs**，main.rs 只调用） | `app.rs::AppContext::build` |
| `run/shell/env` 隔离物化/吸收/环境变量按 provider 分发（表内 codex/kimi 走通用 `IsolatedProvider`；claude 保留专用分支；cursor 明确不支持） | `cmd/run.rs::materialize` / `absorb` / `env_vars` |
| 全局编号（与默认入口渲染顺序必须一致，AGENTS.md #7） | `app.rs::AppContext::list_ordered` |
| 默认入口总流程 | `cmd/default.rs::run` |
| 写操作后余量表（不 sync / 不 AutoSwap / 不拉 daemon） | `cmd/default.rs::print_status_overview` |
| 自动同步 Claude/Codex/Kimi/Cursor 本地激活账号 | `cmd/default.rs::sync_local_active` |
| 账号骨架 → 并发拉 quota + mpsc 渐进渲染；单次 attempt 超时见 `quota.fetch_timeout_ms` | `cmd/default.rs::build_loading_snapshots` / `fill_quotas_progressively`；超时包装在 `quota_query::query_quota_with_retry` |
| 原地刷新渲染 / 全局编号渲染 | `render.rs::InlineRenderer` / `render_to_string` |
| 底层错误压成一行短语（401/429/timeout/network…） | `render.rs::compact_error` |

### 7.9 自动切换决策（`crates/core/src/auto_policy.rs`）

| 职责 | 位置 |
|---|---|
| 拉取状态枚举 Loading/Ready/Failed | `auto_policy.rs::QuotaFetchState` |
| 切换决策（CLI 经 `subswap_core::auto_decide` 调用，即 `decide` 的重导出） | `auto_policy.rs::decide` |

<!-- 该文档整理/压缩于 2026-09-05 -->

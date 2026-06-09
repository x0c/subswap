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
│ - 解析 registry.json  │             │ - keyring + 备份替换    │
│ - OpenAI usage 端点   │             │ - Anthropic usage 端点  │
│ - 同步 CLI/VSCode/App │             │ - 同步 ~/.claude        │
└──────────┬────────────┘             └──────────┬─────────────┘
           │                                     │
┌──────────▼─────────────────────────────────────▼─────────────┐
│ 平台抽象 (crates/core)                                        │
│  - CredentialStore (trait) → KeyringStore (impl)              │
│  - AppPaths (XDG / Library / AppData)                         │
└──────────────────────────────────────────────────────────────┘
```

## 2. 设计模式

| 模式 | 落地位置 | 作用 |
|---|---|---|
| Strategy + Factory | `Provider` trait + `ProviderRegistry` | 多 Provider 多策略，新增 = 加一行注册 |
| Adapter | `providers/codex`、`providers/claude` | 把各订阅的接口差异封进各自实现 |
| Repository | `CredentialStore` trait + `KeyringStore` impl | 隔离 keyring；未来可加加密文件后端 |
| Observer | M4 的 `UsageMonitor` → `AutoSwapPolicy` | 周期采样触发自动切换 |
| Chain of Responsibility | M4 的 `AutoSwapPolicy` 内部 | 阈值 → 限流 → 候选筛选 → 选优 |

## 3. 关键数据流

### 3.1 `subswap`（无参默认入口）

```
① sync_local_active
   └─ claude.import_active() + codex.import_active()
      （读本地 ~/.claude / ~/.codex，upsert registry；失败静默跳过）

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

`find_unique(id)` 支持全局 id 反查（唯一时省略 provider；歧义时用 `<provider>/<id>`）。
默认入口在每行账号前打全局编号（跨 provider 连续，1-based），编号来源是 `AppContext::list_ordered()`
—— 与 `subswap swap N` / `subswap rm N` 共享同一映射，保证「屏幕上看到的第 3 行」就是 `swap 3` 切的那个。
渲染器在 tty 下用 ANSI dim/color 做视觉分层：active 标记 `*` 用 bold cyan、warn 黄、full 加粗红、
其余 ok / 编号 / reset 时间 / 标签均 dim，让用户一眼锁定告警与当前账号。非交互（管道 / 重定向）退化为纯文本。

默认入口的交互要求：
- 不能等所有网络请求结束后才第一次输出；账号列表必须先出现。
- quota 行使用稳定、面向人读的块状字段，例如 `5h [ 99% warn reset in 4h ]`。
- 没有有效数据的窗口不显示；例如 Claude `extra_usage` 缺 utilization 时不输出 `mo=?`。
- reset 默认显示相对时间（`in 69m` / `in 4h` / `in 3d`），避免绝对时间列挤压。

### 3.2 `subswap login <provider>`

```
claude: subswap login claude → claude auth login --claudeai → claude.import_active()
codex:  subswap login codex  → codex login                 → codex.import_active()
                                      └─ registry.set_active(provider, imported_id)
```

设计取舍：
- login 不复刻私有 OAuth 流程，优先委托厂商官方 CLI，降低接口漂移和风控/条款风险。
- 同一账号重新 login 时按 `(provider, id)` 覆盖 keyring 旧凭证，不新增重复账号。
- 登录完成后以官方 CLI 当前激活账号为准，导入 registry 并标记为 active。

### 3.3 `subswap swap [<id|N>]`

```
resolve_account(input):
   ├─ 纯数字 N → list_ordered()[N-1]
   └─ 否则     → find_unique(input)
Provider.activate(id)
   ├─ best-effort refresh（若 token 近过期）
   ├─ spawn_blocking { flock → snapshot → 写文件 → 写 registry }
   └─ 写 audit
```

无参 `subswap swap` 不做切换：只打印 `Usage: ...` + 带编号清单（不查 quota，保持手动入口零网络依赖的不变量）。

**重要**：此路径不依赖 `query_quota`，网络完全不通时仍可用。`subswap rm` 走同一份 `resolve_account` 解析。

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

API 配置仍属于 `claude` Provider，因此列表、编号、`swap`、`rm` 保持一致。它没有 quota，并以
`manual_only` 明确禁止自动切入和自动切出。

### 3.4 `subswapd` daemon（M4）

```
每 defaults::DAEMON_POLL_INTERVAL_MS（默认 60s）：
   ├─ build_snapshots → auto_decide → activate（如需）
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

抽象：`crates/core/src/store.rs::CredentialStore` trait，`compose_key()` 拼
`{provider}:{account}:{field}`。读不存在的条目返回 `Ok(None)`，仅平台/IO 错误才 `Err`。
两种后端实现：

- **`FileStore`（默认装配）**：明文 JSON 单文件 `<data_dir>/credentials.json`，Unix 下 `0600`。
  cli/daemon 在 `AppContext::build()` / daemon `run()` 里默认用它。可挂 `with_legacy_keyring`
  回退：文件未命中某项时从旧 `KeyringStore` 读出并落盘，实现 Keychain→文件的**按需一次性迁移**，
  迁移后该项永不再碰钥匙串。
- **`KeyringStore`**：系统钥匙串后端（见下表），现仅作为 `FileStore` 的迁移回退源保留。

**为什么默认走文件而非钥匙串**：macOS 上每次读写钥匙串 item 都可能弹系统授权框，
重编译/覆盖安装会换应用身份导致**反复弹框**（详见
[troubleshooting/2026-05-29-macos-keychain-prompts.md](../troubleshooting/2026-05-29-macos-keychain-prompts.md)
与 [troubleshooting/2026-06-06-filestore-credential-backend.md](../troubleshooting/2026-06-06-filestore-credential-backend.md)）。
明文文件后端彻底规避此问题，代价是 token 明文落盘（`0600`，与 Codex 的 `~/.codex/auth.json` 同级）。

**`KeyringStore` 多端后端差异（迁移回退源；也影响早期 daemon 保活正确性）**：

| 平台 | keyring 后端 | 进程间可见 | 重启后持久 |
|---|---|---|---|
| macOS | Keychain | ✅ | ✅ |
| Windows | Credential Manager | ✅ | ✅ |
| Linux | `linux-keyutils`（内核 keyring，编译期默认 feature） | ⚠️ 按内核 session 隔离 | ❌ 默认不跨重启 |

Linux 的 keyutils 后端按**内核 session keyring** 隔离。`subswapd` 由 CLI 经
`fork + setsid` 拉起（`crates/cli/src/daemon_spawn.rs`），`setsid` 会进入**新 session**，
因此 daemon 读不到 CLI 在自己 session 写入的条目。**换用 `FileStore` 后此隔离问题一并消失**
（文件对所有 session 可见、跨重启持久），daemon 保活不再受 keyutils 隔离影响。
背景见 [troubleshooting/2026-05-29-daemon-keyutils-session-isolation.md](../troubleshooting/2026-05-29-daemon-keyutils-session-isolation.md)。
推论：token 自愈仍不只依赖 daemon；查询/切换路径也能 best-effort 刷新。

### 4.2 配置目录（元数据，明文）

| 平台 | 路径 |
|---|---|
| Linux | `$XDG_CONFIG_HOME/subswap/` 或 `~/.config/subswap/` |
| macOS | `~/Library/Application Support/dev.subswap.subswap/` |
| Windows | `%APPDATA%\subswap\subswap\config\` |

文件：
- `registry.toml`：账号元数据列表（label、created_at、priority、provider extra）。
- `state/snapshots/<ts>/`：切换前快照。
- `state/state.json`：当前激活账号、daemon 状态、冷却计时。
- `audit.log`：切换审计。

### 4.3 Provider 私有目录（沿用上游）

- Codex：`~/.codex/accounts/registry.json` + `~/.codex/sessions/`
- Claude：`~/.claude/`

subswap 切换时**写**这些上游目录，但**只读不存** token 元数据（token 已在凭证仓库 `FileStore`）。

## 5. 扩展新 Provider 的步骤

1. 新建 `crates/providers/<id>/` crate，依赖 `subswap-core`。
2. 实现 `Provider` trait（`list_accounts / activate / query_quota / client_targets`）。
3. 在 `crates/cli/src/app.rs::AppContext::build()` 注册一行。
4. 在 `crates/cli/Cargo.toml` 加依赖；在 `sync_local_active()` 加 import_active 调用。
5. 在 `docs/PROVIDER_KNOWLEDGE_BASE.md` 补该 Provider 的接口/坑笔记。

不要在 `core` 里写任何 Provider 特定逻辑。

## 5.5 数值调优常量的管理

**运行期值**走 `crates/core/src/settings.rs::current()`，由 `<config_dir>/config.toml` 加载（热生效）；
**编译期默认值**仍集中在 `crates/core/src/defaults.rs`（`Settings::default()` 从这里读）。
provider / cli / daemon 都禁止硬编码阈值、时间窗口、百分比。

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

调字段：用户改 `config.toml`；改默认值改 `defaults.rs` 一处 + AGENTS.md 不变量 #4 同步当前值。
完整说明见 [docs/CONFIG.md](../CONFIG.md)。

### Daemon 空闲退避

`daemon` 主循环每轮开头：
1. `settings::reload_from_file()` 拿最新 config。
2. 扫所有 provider `client_targets().probe_path` 的 mtime；最近一次活动距今 ≥ `idle_threshold_ms` → 用
   `idle_poll_interval_ms`，否则用 `poll_interval_ms`。
3. probe 文件不存在 / 拿不到 mtime → 按「空闲」处理（保守，避免凭空高频轮询）。

这套机制让用户长时间不用 AI 时 daemon 自动放慢；下次官方 CLI 调 API 触发 token 写回 → 立刻回到活跃节奏。

## 5.6 风控边界

自动切换不能通过高频请求“探测”额度或制造 429。CLI 无参入口只在用户主动执行时采样一次；
daemon（M4）按 `DAEMON_POLL_INTERVAL_MS` 低频轮询，并在失败后退避。未来 429 立即切换只能来自
真实客户端 hook / 本地 IPC 上报，不能靠更密集的 usage 请求实现。

默认 CLI 不用持久 quota cache 来掩盖实时查询失败。原因：缓存会降低请求频率，但容易让用户误以为当前额度仍有效；
限流/认证失败应明确显示短状态（如 `rate limited` / `auth failed`），请求频率问题应通过低频采样、渐进渲染、
daemon 退避或真实客户端 hook 解决。

## 6. 错误处理

- `core::error::Error` 是统一错误枚举。Provider 内部用 `anyhow::Error` 处理细节，通过 `Error::Other` 或 `Error::Provider(String)` 暴露。
- CLI 层用 `anyhow::Result` + `with_context` 给用户加上下文。
- 错误绝不静默吞掉；`query_quota` 失败时返回 `Err`，CLI 自行决定是否降级。

## 7. 关键代码路径地图

> 目的：核心流程「在哪个文件、哪个函数」一次性查到，避免每次现读源码。函数名比行号稳定，故只记函数名。
> 改动这些流程时同步更新本表。

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

| 职责 | 函数 / 文件 |
|---|---|
| 导入本地激活账号 / 切换 / 拉 quota | `lib.rs::import_active` / `activate` / `query_quota` |
| usage 解析（字段不稳定，容错） | `openai_usage.rs` |
| `~/.codex/auth.json` opaque 透传 schema | `codex_files.rs` |
| 路径 | `paths.rs` |

> Codex **不做** token 刷新（设计理由见 PROVIDER_KNOWLEDGE_BASE.md「Codex Token 刷新（subswap 不做）」）。

### 7.5 daemon（`crates/daemon/src/`，Unix-only）

| 职责 | 位置 |
|---|---|
| 主循环 + 空闲退避选周期 | `unix.rs::decide_next_interval` 及主循环 |
| 每账号 `query_quota` 收快照（失败记 `QuotaFetchState::Failed`） | `unix.rs`（snapshot 收集） |
| Claude token 后台保活（遍历所有账号调 `refresh_if_near_expiry`） | `unix.rs::keep_claude_tokens_alive` |
| 单实例 PID 文件锁 | `unix.rs::open_pid_lock` / `write_pid` |
| CLI 无感拉起（`fork + setsid` + stdio 重定向到日志） | `crates/cli/src/daemon_spawn.rs::ensure_daemon_running` / `spawn_detached_daemon` |

### 7.6 CLI（`crates/cli/src/`）

| 职责 | 位置 |
|---|---|
| `AppContext`（注册所有 provider，**定义在 app.rs**，main.rs 只调用） | `app.rs::AppContext::build` |
| 全局编号（与默认入口渲染顺序必须一致，AGENTS.md #7） | `app.rs::AppContext::list_ordered` |
| 默认入口总流程 | `cmd/default.rs::run` |
| 自动 import 本地激活账号 | `cmd/default.rs::sync_local_active` |
| 账号骨架 → 并发拉 quota + mpsc 渐进渲染 + 整体超时（`quota.fetch_timeout_ms`） | `cmd/default.rs::build_loading_snapshots` / `fill_quotas_progressively` / `mark_pending_as_timed_out` |
| 原地刷新渲染 / 全局编号渲染 | `render.rs::InlineRenderer` / `render_to_string` |
| 底层错误压成一行短语（401/429/timeout/network…） | `render.rs::compact_error` |

### 7.7 自动切换决策（`crates/core/src/auto_policy.rs`）

| 职责 | 位置 |
|---|---|
| 拉取状态枚举 Loading/Ready/Failed | `auto_policy.rs::QuotaFetchState` |
| 切换决策（CLI 经 `subswap_core::auto_decide` 调用，即 `decide` 的重导出） | `auto_policy.rs::decide` |

# Provider 知识库

各 Provider 的上游接口、本地文件、认证字段等代码不能表达的事实。新加 Provider 按本文结构补一节。

## §0 目录索引

| 节 | 内容 |
|---|---|
| 额度语义（跨 Provider 统一约定，先读） | 已用% vs 余量分层约定、`Quota.used` 语义、CLI 展示 UX |
| Claude / Anthropic | OAuth 常量与端点、Usage 字段与 429/401 语义、keychain 激活、自定义 API 与 BillingKind |
| Codex / ChatGPT | Usage 端点与不稳定字段、auth.json 透传、切换生效边界、官方额度通道与 refresh 轮换 |
| Kimi / Moonshot | 凭证路径、JWT 元数据、刷新与 Usage 端点、官方锁协调、测试隔离与登录方式 |
| OpenCode Console official sign-in and Go monitoring | Console OAuth 登录识别/委托、Go 额度监控、secret 隔离约束 |
| OpenCode Go | auth.json 的 opencode-go 项抽取/覆盖、用量端点、隔离运行、登录方式、开源号池辨析 |
| Command Code | auth.json 路径、用量端点、隔离运行、登录方式 |
| Cursor | SQLite 状态与跨平台路径、登录导入切换事务、额度三池与刷新边界、自动切换约束 |
| 文件型 OAuth 切换共享引擎 | `FileBlobProvider` 引擎与 `FileBlobRuntime` adapter 差异点 |

---

## 额度语义（跨 Provider 统一约定，先读）

**数据层与状态层一律「已用百分比」；仅 CLI 展示层转「余量」。**

| 层 | 语义 | 位置 |
|---|---|---|
| 上游字段 | **已用 %**（0~100） | Claude `utilization`（`oauth.rs::WindowUsage`）；Codex `used_percent` / `percent`（`openai_usage.rs`）；Cursor `autoPercentUsed` / `apiPercentUsed` |
| `Quota` 模型 | **已用**：`used`(0~100) + `limit`(固定 100) | `make_quota`（claude）/ `query_quota`（codex）把已用% 写入 `Quota.used` |
| 状态判定 | 基于**已用%**：`used ≥ quota.warn_pct`(默认 90)→Warn，`≥ quota.exhausted_pct`(默认 100)→Exhausted | `QuotaStatus::from_percent` |
| CLI 展示 | 统一**余量** `{limit - used}% left`（含 Cursor `1st` / `API`） | `render.rs::format_quota_compact` |

**既定 UX（勿改回）**：默认只显示 `{N}% left`，不打印 ok/warn/full；严重程度靠数字 + 颜色。改展示时**不要翻转 `Quota.used` 语义**。

---

## Claude / Anthropic

### OAuth 公开常量

| 项 | 值 | 备注 |
|---|---|---|
| Client ID（默认） | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` | 公开值，非 secret |
| 环境变量覆盖 | `SUBSWAP_CLAUDE_OAUTH_CLIENT_ID` | 上游变更逃生口 |
| 实现位置 | `crates/providers/claude/src/oauth.rs::DEFAULT_CLIENT_ID` | |

### 上游 HTTP 端点

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `https://api.anthropic.com/api/oauth/usage` |
| Token 刷新 | POST | `https://platform.claude.com/v1/oauth/token` |

公共请求头：
- `Authorization: Bearer <access_token>`（usage）
- `anthropic-beta: oauth-2025-04-20`（usage；上游调整须同步 `BETA_HEADER`）
- `User-Agent: subswap/<version>`

Token 刷新体：`{"grant_type":"refresh_token","refresh_token":"...","client_id":"..."}`

### Usage 响应字段（subswap 关心的）

- `five_hour.utilization` — 0~100 已用%；`five_hour.resets_at` — ISO8601
- `seven_day.utilization` / `seven_day.resets_at`
- `extra_usage.utilization` / `extra_usage.resets_at` / `extra_usage.monthly_limit` / `extra_usage.used_credits`
  （`extra_usage` 无 `utilization` 时不产生 `Month` 窗口，直接跳过，与 OpenCode 缺失窗口跳过一致；
  否则常驻的 `limit == 0 / Unknown` 月窗口会把全员耗尽回退池永久挡死）

`utilization` 固定按 0~100 已用% 解析。**小于 1 仍表示不到 1% 已用，禁止当 0~1 比例放大**（否则 `0.97%`→`97%`）。

**未公开接口，字段类型会在 Claude Code 版本间漂移，必须逐字段宽容解析。**
`oauth.rs::lenient`：任一字段解不出 → `None`，绝不能整份崩。`Option<T>` 只容忍缺字段，**不容忍类型变化**。
已知漂移（2026-07）：`extra_usage.used_credits` 整数→小数（伴 `currency` / `decimal_places`）→ `monthly_limit` / `used_credits` 一律 `f64`；`extra_usage` 可能无 `resets_at`；未知代号窗口（`tangelo` 等）可忽略。致命的是**已知字段类型变化**。
全文与手法：[troubleshooting/2026-07-26](troubleshooting/2026-07-26-claude-usage-schema-drift-bad-response.md)。

### Usage 异常状态码（429 ≠ token 失效）

**429 是 usage 端点真实极严限流**（有效 token 间隔约 4s 仍 `200→429→429` + `retry-after`），不是鉴权伪装。完整排查：[troubleshooting/2026-06-14](troubleshooting/2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md)。

Research update (2026-10-09): the historical one-request-per-minute observation is not an official allowance. Upstream reports include hour-scale blocks, busy-account 429 across fresh tokens/request identities, and optional official statusline readings that add no usage request. An expired active credential can coexist with a cached 429; 429 does not validate the credential. Current code also drops `Retry-After`. Evidence, limitations, and proposed alternatives: [persistent Claude usage 429](troubleshooting/2026-10-09-claude-persistent-usage-429.md). The research does not enable a new query or refresh path.

| 信号 | 端点/状态 | 含义 | 处理 |
|---|---|---|---|
| `429 rate_limit_error` | usage 429 + `retry-after` | 约**每账号每分钟 1 次** | 缓存节流，**不是**重登 |
| `invalid_grant` | refresh `/v1/oauth/token` 400 | parked refresh 已死 | 死 token 守卫 + 重登 |
| `401` | usage 401 | active live 过期、Claude Code 未刷 | 开一次 Claude Code；subswap 不刷 active；失败退避最多保留 90s |
| `bad response` | usage **200** 但 parse 失败 | 响应结构漂移 | 补宽容解析；能走到 parse = 鉴权/限流 OK |
| 空 access token | 本地凭据 | 登录/切换中间态回灌不完整 | 不发 usage，显示 `needs re-login`；回灌保留 store 完整副本 |

**缓存节流**（`crates/cli/src/cmd/default.rs` + `crates/daemon/src/unix.rs::build_snapshots`）：
daemon 与 CLI **共用** `quota_cache.json`；`QuotaCache::fresh()` 比 `settings.quota.min_refresh_interval_ms`（默认 90s，> daemon 60s 轮询）新则复用、不打端点 → 每账号 ~90s 一次。

**失败退避**（`QuotaCache::record_failure` / `in_failure_backoff`）：
成功缓存不覆盖失败路径。失败记 `failures`，退避 `min_refresh × 2^(连续失败-1)`，封顶 `settings.quota.failure_backoff_max_ms`（默认 15 分钟）；成功清零。
**401/403 例外封顶为一个 `min_refresh`（默认 90s）**：避免原生已刷活而 subswap 仍挂旧 401。
**新增任何 quota 查询路径必须同时接失败退避。**

- 勿手动连发 curl usage 打空限流桶。
- `oauth.rs::fetch_usage` 保留原始状态码；CLI 压成 `429 rate limited`；stale 行 `(cached ~Xm · 429 rate limited)`；排查看 `--log debug`。

### 本地激活文件

| 路径 | 用途 |
|---|---|
| `~/.claude/.credentials.json` | OAuth 凭证；Claude CLI 实际读取 |
| `~/.claude.json` | 新版全局配置；含 `oauthAccount` |
| `~/.claude/.config.json` | 旧版全局配置；存在则优先 |

`.credentials.json` 关心字段：

```json
{
  "claudeAiOauth": {
    "accessToken": "...",
    "refreshToken": "...",
    "expiresAt": <epoch_ms>,
    "refreshTokenExpiresAt": <epoch_ms>,
    "scopes": ["user:inference"]
  }
}
```

其他字段 `#[serde(flatten)]` 透传。

**`refreshTokenExpiresAt`**（Claude Code 2026-07-09 起，实测约 30 天）：refresh **会自然过期**。`refresh_token_expired()` 提前判掉、不发请求，透出 `needs re-login`。字段缺失（老客户端）不判过期。与 [live capture 覆写 refresh](troubleshooting/2026-06-18-live-capture-clobbers-refresh-token.md) 是两类重登原因——先看该字段是否到期。

`oauthAccount`：`emailAddress` / `accountUuid` / `organizationUuid` / `organizationName`。

### 切换 (activate)

1. 整段重写 `~/.claude/.credentials.json`（原子，0o600）
2. 只替换 `~/.claude.json` 的 `oauthAccount`（保留 `projects` 等）
3. `fs2::FileExt::lock_exclusive` 于 `<claude_home>/.subswap.lock`

### 删除（rm 连带原生登出）

删的是当前 live OAuth 账号时：macOS 经 `/usr/bin/security` 删除 `Claude Code-credentials` item（不存在则忽略），删除 `.credentials.json`（不存在则忽略），最后移除全局配置里的 `oauthAccount`（保留其他字段）；三步后复核 live 已无该账号，否则报错且不清本地。自定义 API 账号没有 live 常驻，parked 删除只清本地（active API 本来就删不掉）。

Token 预刷新 **best-effort**：`expiresAt` 在 5 分钟内且 keyring 有 `refreshToken` 时刷；失败仅 warn，不阻塞切换（不变量 #1）。

**Token 保活分工**：非活跃 `access_token` 只在凭证仓库 → **daemon 后台保活**（扫描 `expires_at`，临近过期且有 `refresh_token` 调 Anthropic OAuth + 写回）。不暴露 `subswap refresh`。active 绝不刷（见下「Refresh token 轮换」）。

### Claude Code 自定义 API

`~/.claude/settings.json` 的 `env` 可配兼容端点。

| 预设 | 端点 | 认证 | 模型 |
|---|---|---|---|
| DeepSeek | `https://api.deepseek.com/anthropic` | `ANTHROPIC_AUTH_TOKEN` | Opus/Sonnet/Haiku → DeepSeek 模型映射 |
| Kimi 编码 | `https://api.kimi.com/coding` | `ANTHROPIC_API_KEY`（`x-api-key`） | 按会员档解锁、**不自动路由** |

Kimi 模型档：`kimi-for-coding` = 各档通用 K2.7；`k3` / `k3[1m]` 仅 Moderato+ / Allegretto+；`kimi-for-coding-highspeed` 仅 Allegretto+。
`add-api` Kimi 预设让用户**分别选 Opus/Sonnet/Haiku**；默认与非交互缺省均为 `kimi-for-coding`；`--opus-model` / `--sonnet-model` / `--haiku-model` 可覆盖。

与 OAuth 共用 `provider = "claude"`，元数据：

```toml
[accounts.extra]
kind = "api"
manual_only = true
```

- API Key → `CredentialStore(field=api_key)`；registry 只存端点与模型映射。
- 激活合并写 `settings.json.env`，保留 hooks/permissions/plugins/其他 env。
- `.subswap-api.json` 存 active API id 与切入前受管 env 恢复值；文件与快照 `0600`。
- 切回 OAuth 时恢复受管 env 并删标记。
- API active 时 Key 明文在 `settings.json`（上游机制边界）。
- `query_quota` 返回空列表；`manual_only` → 只能手动切入，active 时自动换号停用。

### 账号计费方式（BillingKind，v0.3.23+）

`Account.billing()` 读 `extra["billing"]`；供下游（如 OpenConductor）判断「按量花钱」。新增适配器只需在 `extra` 如实标注。

| 枚举值 | `extra["billing"]` | 语义 |
|-------|-------------------|------|
| `Flat` | 缺省 | 固定费率订阅 |
| `Metered` | `"metered"` | 按量计费 |
| `Unlimited` | `"unlimited"` | 不限量 |

**向后兼容**：早于 v0.3.23 的 API 账号无 `billing` 但有 `kind = "api"` → `Account::billing()` 视为 `Metered`。

JSON（`subswap list --json`）序列化：`"flat"` / `"metered"` / `"unlimited"`；默认入口摘要含 `billing`。

写入：`add-api` 三选一或 `--billing <value>`；缺省 `metered`。

### Claude 自定义 API 的模型角色

只暴露 Opus / Sonnet / Haiku（`--opus-model` 等）。激活时 Sonnet→默认模型字段，Haiku→子任务模型字段（内部兼容映射）。

**`BillingKind` 不进 auto_policy**：`auto_policy.rs` 的候选筛选与 `compare_candidates` **不**使用该字段，只供下游判断，不改变自动切换决策。

---

## Codex / ChatGPT

### 上游 HTTP 端点

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `https://chatgpt.com/backend-api/wham/usage` |
| 账户元数据 | GET | `https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27` |

请求头：`Authorization: Bearer <access_token>`；`ChatGPT-Account-Id: <chatgpt_account_id>`；浏览器风格 `User-Agent`。

active **不首选**兼容 HTTP：先经官方 `codex app-server` JSONL 调 `account/rateLimits/read`，优先复用 `<CODEX_HOME>/app-server-control/app-server-control.sock`。parked 无法安全物化完整官方认证 → 仍走 `wham/usage`。

The app-server response is usable only when its `accountId` matches the registry account's `chatgpt_account_id`. A control socket may belong to a Codex process that started before a swap and still holds the previous login. Connect with WebSocket directly, initialize with the official handshake, and preflight `account/read {refreshToken:false}` using optional `workspaceRouting.chatgptAccountId`. Reject a known stale identity before requesting quota; always verify the final quota `accountId`. Missing or different response identity goes directly to the compatible read-only query using the selected account's access token, without spawning another process or refreshing the stale account. See [the cloned-quota investigation](troubleshooting/2026-09-25-codex-quota-cloned-after-swap.md) and [active-query latency](troubleshooting/2026-10-09-codex-active-quota-loading.md).

### Usage 响应字段（不稳定）

`openai_usage::normalize()` 宽松解析：

- 顶层与 `usage / quota / limits` 嵌套都尝试；`primary / secondary` 与 `rate_limit.primary_window / secondary_window` 任意层级递归识别
- **禁止**递归进入 `additional_rate_limits` / `code_review_rate_limit` / `model_usage`（会多出第二个 `7d`、误判周额度耗尽）。见 [2026-09-05 Codex 两个 7d](troubleshooting/2026-09-05-codex-duplicate-7d-from-additional-rate-limits.md)
- 窗口分钟：`300` → 5h，`10_080` → 7d，`28*1440..=31*1440`（如 43200/43800）→ `mo`，其余 → `Custom`
- 候选字段：`used_percent / percent / used / limit / resets_at / reset_at / window_minutes / limit_window_seconds`
- 全不可解析 → `Quota { status: Unknown }` 而非 `Err`
- 实时成功但字段不可识别且有新鲜 `last_usage` → 本地缓存兜底；有效期见 `defaults::CODEX_USAGE_CACHE_MAX_AGE_MS`

### Rate limit reset credits（只读展示，不参与自动切换）

Codex 会定期赠送限额重置道具（banked reset，可把 5h+7d 一次清零，有 30 天左右过期时间）。
subswap 只读展示数量与最早过期，不代为消耗。

| 用途 | 方法 | URL |
|---|---|---|
| 重置明细 | GET | `https://chatgpt.com/backend-api/wham/rate-limit-reset-credits` |

- 请求头与 usage 相同：`Authorization: Bearer <access_token>` + `ChatGPT-Account-Id` + 浏览器风格 `User-Agent`。
- 计数零请求：`wham/usage` 的 `rate_limit_reset_credits.{available_count, applicable_available_count}`
  （app-server `account/rateLimits/read` 为 camelCase `rateLimitResetCredits.availableCount`，可同时带 `credits` 明细）。
  `available_count == 0` 或缺失 → 不发明细请求、不显示 reset 列。
- Details first reuse inline `credits[]` from the quota response. Active accounts never make an extra detail HTTP request; missing details show the known count with unknown expiration. Parked accounts may query the detail endpoint only when the count is positive and inline rows are absent, bounded by `codex.reset_details_timeout_ms` (default 1000 ms; zero skips).
- Only `status == "available"` rows are used. Parse snake_case RFC3339 `expires_at` and official camelCase epoch-second `expiresAt`. Preserve the authoritative count if rows are capped, empty, or invalid; only display earliest expiration when all available rows are present.
- 映射：`available > 0` 时多一个 `QuotaWindow::ResetCredits` 窗口（`used` = 可用数，`limit` = 0，
  `reset_at` = 最早过期，`note` = 标题与过期摘要）；`0` 时整列隐藏（多数账号无 reset，避免噪音）。
  默认入口标签 `RS`（如 `RS [1 reset exp in 29d]`）；`subswap --json` 的 quotas 数组带同字段。
- 频率与失败：随正常 quota 采样走同一份 90s 缓存节流（daemon 与 CLI 共用 `quota_cache.json`）；
  明细失败只降级为「有数量、无过期」，不影响 5h/7d；429 走统一失败退避、不重试。
- 自动切换：reset 窗口不参与判定（`quota_gates_auto_swap` 排除，`limit == 0` 也不计入可用/耗尽统计）——
  耗尽号即使有 reset 仍视为耗尽、可被自动切走。subswap 绝不调用任何 consume/redeem 接口。

### 本地激活文件

| 路径 | 用途 |
|---|---|
| `~/.codex/auth.json` | 当前激活；**CLI / VSCode / 桌面端共用** |

切换 = 原子重写这一文件。**新启动**的 CLI / 扩展 / 桌面端都会读到新号；**已在跑的 Codex 进程不会自动跟上**（见下节「切换生效边界」）。

### 切换生效边界（官方不热读 · 2026-09-11 裁定）

**【裁定】** subswap 的 Codex 手动 `swap` 与自动换号**都已实现**（默认入口 `try_auto_swap_ready_provider`、daemon `run_cycle`、共享引擎 `FileBlobProvider::activate`）。用户感觉「没有自动切 / 切了不生效」，优先按下列边界排查，**禁止**当成「Codex 自动切号未落地」去重做。

官方 Codex `AuthManager`（`codex-rs/login`）启动时把 `auth.json` 读进内存，并写明：外部改写磁盘 **不会**被观察，除非进程显式 `reload()`。自愈路径里的 `reload_if_account_id_matches` 在 **账号 ID 变化时故意跳过**——正好挡住「换到另一个号」的热灌。因此：

| 场景 | 实际效果 |
|---|---|
| `subswap` / `subswapd` 切号成功 | live `auth.json` + registry 已是新号 |
| 已打开的 Codex CLI / IDE 会话 | 继续用内存旧号，须**重启 Codex**（或重载 IDE 窗口后再开对话） |
| `subswap run codex <账号>` | 新进程 + 私有 `CODEX_HOME`，立刻用目标号；不动全局 live |
| 指望官方热读换号 / 捆绑社区 Codex 热补丁 | **禁止**：官方设计不允许；非官方补丁跟版本、签名、安全均不可控 |

同类开源换号工具（写 `auth.json` 的）同样要求重启。若要做体验补丁，只能走「切号后提示或协调重启」（对标 Cursor 退出再拉起），**不是**进程内热切换。

易被误判成「没自动切」的旁因：

1. **macOS 默认不拉起 `subswapd`**（避钥匙串弹窗）；后台轮询需 `SUBSWAP_AUTO_DAEMON=1`。平时只有跑无参 `subswap` 才会采样并自动切一次。见 [CLI.md](CLI.md)。
2. **自动策略只对小时级窗口做阈值触发**；Codex 月度等长窗口接近阈值不切，只有明确 `Exhausted` 才硬阻断。见 [AUTO_SWAP_DESIGN.md](design/AUTO_SWAP_DESIGN.md) §1.1。

排障入口：[troubleshooting/2026-09-11](troubleshooting/2026-09-11-codex-swap-requires-restart.md)。

### Codex 官方额度通道与刷新边界

subswap **不实现 OpenAI OAuth 去抢刷当前号（active）**。active 刷新只委托官方 app-server：

1. Control socket present: connect directly with WebSocket, initialize, preflight optional workspace identity without forced refresh, then read quotas and verify `accountId`. An account mismatch immediately uses the compatible read-only HTTP query; do not ask a stale daemon to refresh. `codex app-server proxy` only relays raw bytes and cannot turn JSONL into WebSocket.
2. 无 socket、确认无普通 Codex 进程 → 可短暂 `codex app-server --stdio`；先读额度；仅官方认证失败时 `account/read {refreshToken:true}` 强刷一次再重试额度一次。
3. 无 socket、但普通 Codex 在跑 → 仍可启临时 app-server，用 `0600` 临时 `CODEX_HOME`，只复制 live `auth.json` **并清空 refresh token**（能用现有 access，绝不与运行中 Codex 抢刷）。
4. 官方不可用/认证失败/方法不支持 → 回退 `wham/usage`；官方 429 与其它服务错误**原样返回，禁止二次回退再打**。

parked 查额度前由引擎调 `runtime.refresh()`：`CodexRuntime::refresh` → `oauth::refresh_parked_blob`——**直连** `POST https://auth.openai.com/oauth/token`（Codex 公开 `client_id`，与 `codex-switch` / `shuvquota` / `codex-multi-auth` 同路径），合并新 access/refresh/id 进完整 `auth.json` blob，经引擎 `store.set` 写回仓库，再用新 access 打 `wham/usage`。access 仍在 `REFRESH_SLACK_MS` 外则跳过；无 refresh / 传输失败则降级。仅 `refresh_token_reused` / `invalid_grant` / 401·403 等终态标 `DeadToken`（进程内 refresh 指纹守卫禁止再打第二枪）。  
**禁止**只物化 access、清空 refresh 去刷停用号；**禁止**用临时 app-server 刷停用号却漏吸收（会制造 `refresh_token_reused`）。见 [troubleshooting/2026-09-07](troubleshooting/2026-09-07-codex-parked-quota-401.md)。

外层 `quota.fetch_timeout_ms`（默认 20s）须盖住本会话上限；过短 → `quota fetch timeout` → 默认入口 `timeout after N attempts` 回落旧缓存。Kimi active 401 自愈（官方锁 + 持锁刷新）同受此超时约束。

排查：当前号 401 → [troubleshooting/2026-07-09](troubleshooting/2026-07-09-codex-quota-401-despite-working-cli.md)；停用号 401 → [troubleshooting/2026-09-07](troubleshooting/2026-09-07-codex-parked-quota-401.md)。

### Refresh token 轮换与 capture-on-leave（核心安全约束）

**两边 refresh 都是一次性轮换。** 与原生客户端各持一份并各自刷 → `refresh token already used` 强制重登（[troubleshooting/2026-06-08](troubleshooting/2026-06-08-codex-refresh-token-already-used.md)）。

**不变量：active 只能在原生认可的协调机制内轮换。** Claude/Cursor active 只读不刷；Codex **active** 只委托 app-server；Kimi 仅识别官方锁并成功持锁时自愈。parked 可由 subswap 按各 Provider 串行化边界刷新（Codex parked = 直连 OAuth）；Cursor 用跨进程文件锁。

1. **Capture-on-leave**：`Provider::activate` 覆盖 live 前，读 live → 找受管 owner → 回写 store（Codex/Kimi 共享引擎，Claude/Cursor 各自实现）。所有 swap（手动 + daemon）唯一经 `activate`。找不到 owner 跳过（best-effort）。
   Claude 重复切换当前账号：只回灌并返回，禁止用 store 陈旧 token 盖回 live。
   - **覆盖前比较新旧 access/refresh；禁止用缺字段快照覆盖字段完整的快照**（[troubleshooting/2026-06-18](troubleshooting/2026-06-18-live-capture-clobbers-refresh-token.md)）。
     Claude 缺 refresh → 合并保留旧 refresh、只跟进非空新 access；缺 access → 整段保留 store；Codex 命中 → 整段跳过回灌（opaque blob，不做字段合并）。
2. **Claude active 绝不轮换**：
   - `refresh_if_near_expiry` 开头 `active_account_id()` 命中 → `Ok(false)`；daemon 保活只对 parked。
   - `query_quota` 401 自愈：凭证来自 store（parked）才刷；来自 live（active）直接错误，交 Claude Code。
   - macOS active 凭证优先 Claude Code Keychain；`.credentials.json` 只是兼容副本。

一般 quota 遇 `401` / `403` / `429` 不盲目重试；仅 Codex 官方 app-server、持官方锁的 Kimi active 恢复、Cursor active 重读到更新 live access，允许各重试一次。**429 永不重试或切换通道。**

**capture-on-leave 缺口 + 两道补救**：绕过 subswap 在 Claude Code 内登录/切换 → store 陈旧 → parked 后 daemon 刷死 token 成风暴：

1. **死 token 守卫**（`ClaudeProvider.dead_refresh`，进程内）：`invalid_grant` → 记 refresh 指纹；`refresh_if_near_expiry` / `query_quota` 401 自愈命中则跳过；指纹变化自动恢复。quota 错误含 `re-login` → CLI `needs re-login`（`render.rs::compact_error`）。**只判 `invalid_grant`，网络/超时不判**。
2. **持续回灌**（capture-on-arrival）：daemon 每轮 `ClaudeProvider::reconcile_active_from_live()`（=`capture_live_into_store`，只 live→store，不刷新、不写 live）。缩小缺口但**无法 100% 消除**；彻底恢复仍需重登。

改 `activate` / keepalive / `query_quota` 自愈时须维持：不在后台刷 active、不把陈旧 token 写回 live。

### auth.json schema 不稳定（透传策略）

不绑定具体 schema（经历过 v2→v3→v4）：

- 整段当 **opaque blob** 存 CredentialStore
- 只解析元数据：`account_key / email / alias / chatgpt_account_id / chatgpt_user_id / account_name / plan`
- `access_token` 仅 quota 路径解析：`extract_access_token()` 宽松递归

API-key 型 `auth.json` 示例：

```json
{
  "OPENAI_API_KEY": "...",
  "last_refresh": "...",
  "tokens": { "account_id": "..." }
}
```

无扁平 `account_key/email` 时：优先 `tokens.id_token` JWT payload 的 `email` 作 id/label；`tokens.account_id` → `ChatGPT-Account-Id`。皆缺 → API key 本地指纹作去重 id（指纹不得替代 secret；完整 `auth.json` 仍只存 CredentialStore）。

### 切换 (activate)

1. 整段重写 `~/.codex/auth.json`（原子，0o600）
2. `fs2::FileExt::lock_exclusive` 于 `<codex_home>/.subswap.lock`
3. **不**向运行中的 Codex 发 reload / 不代为重启（当前边界）；生效见上节「切换生效边界」

### 删除（rm 连带原生登出）

删的是当前 live 文件所属账号时一并删除 live 文件（持 `.subswap.lock`），否则只清本地。live 文件只装当前登录，没有多凭证库，parked 删除天然不复活。

### 与其他本地账号工具共存

- 他工具可能维护 `~/.codex/accounts/registry.json` + `accounts/<key>/auth.json`
- subswap **不读不写**；元数据在 `<config_dir>/registry.toml`
- 可共存，勿混管同一账号

---

## Kimi / Moonshot

共享引擎第二个文件型 provider：`crates/providers/kimi/`。凭证整段 opaque JSON blob（同 Codex 哲学）。

### 本地凭证路径

| 项 | 值 |
|---|---|
| 工作目录 | `KIMI_CODE_HOME` > `~/.kimi-code` > `.kimi-code`（`paths.rs::kimi_home`） |
| 当前激活凭证 | `<home>/credentials/kimi-code.json`（`paths.rs::active_cred_path`） |

### 令牌与元数据（JWT，无 email）

`kimi_files.rs::parse_metadata` / `decode_jwt_payload`：

- `access_token` JWT：`user_id` / `client_id` / `scope`，约 **15 分钟过期**
- `refresh_token` 约 **30 天、单次轮换**（同 Codex 风险）
- `primary_id` / `label` = `user_id`；`scope` → `registry.toml` `extra["scope"]`（仅展示）
- `dedup_key` 恒 `None`（`user_id` 稳定，无需 Codex 式额外去重）

### 刷新端点

| 用途 | 方法 | URL |
|---|---|---|
| Token 刷新 | POST | `{KIMI_CODE_OAUTH_HOST:-https://auth.kimi.com}/api/oauth/token` |

- body `form-urlencoded`：`client_id`（旧 access JWT claim）、`grant_type=refresh_token`、`refresh_token`
- 缺 `client_id` / `refresh_token` → `RefreshOutcome::Unsupported`
- `401` / `403` 或 `error == "invalid_grant"` → `RefreshOutcome::DeadToken`
- 成功合并新 `access_token` / `refresh_token` / `scope` / `token_type` / `expires_in`，按 `expires_in` 写 `expires_at`（epoch 秒）
- 实现：`oauth.rs::refresh_blob`

### Usage 端点与窗口映射

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `{KIMI_CODE_BASE_URL:-https://api.kimi.com/coding/v1}/usages` |

- `Authorization: Bearer <access_token>`
- 数值字段常为**字符串**（`kimi_usage.rs::to_u64` 兼数字）；`used` 缺失 → `limit - remaining`
- `reset_at` ← `resetTime`（ISO8601 RFC3339）
- 窗口：顶层 `usage` → `QuotaWindow::SevenDay`；`limits[]` 按 `window.duration` + `window.timeUnit`（`MINUTE`/`HOUR`/`DAY`）→ 分钟；`duration:300, TIME_UNIT_MINUTE` → `FiveHour`；`10_080` 分钟 → 7d；其余 → `Custom`
- 实现：`kimi_usage.rs::fetch_quota_with_active_recovery`（底层 `fetch_quota_at` / `parse_usages` / `window_from_minutes` / `minutes_of`）

### active 401 的官方锁协调

仅在**能确定当前 Kimi 版本官方跨进程锁协议**时，才在同一把锁内重读 live、必要时刷一次、原子落盘、再重试 usage 一次：

| Kimi CLI 世代 | 官方协调机制 | subswap 行为 |
|---|---|---|
| 新 TS 0.x（`--version` 裸 semver） | Unix `oauth/kimi-code.lock/` proper-lockfile 目录锁 | 兼容目录锁、1 秒续租；连续 >10 秒未续租才原子改名清 stale |
| 新 TS 0.x（Windows） | 无等价跨进程锁 | active 保持 401，**绝不刷新** |
| 旧 Python `>= 1.31.0` | `credentials/kimi-code.lock` 文件锁 | 同路径 + flock |
| 旧 Python `< 1.31.0`、版本未知/无法执行 | 无可证明兼容锁 | 安全降级，active 保持 401 |

`KIMI_DISABLE_OAUTH_LOCK=1` 时 subswap 也必须禁用 active 刷新。持锁后先重读 `kimi-code.json`：已轮换 → 复用最新 access；账号不匹配 → 退出；仍是本次失败那枚才刷新。

刷新成功须在释放锁前 tmp+rename 原子替换 live；`invalid_grant` / 401 / 403 只存 refresh SHA-256 指纹（不存 secret）；同指纹不再发；token 变化后恢复。网络/超时不判死。

（强行启停 kimi TUI「等它自刷」不构成稳定契约，且会打断会话；官方锁才是不并发消耗一次性 refresh 的可证明边界。）

### 测试环境变量

`KIMI_CODE_OAUTH_HOST` / `KIMI_CODE_BASE_URL` 覆盖刷新与 usage base（集成测试 mock）；同 `KIMI_CODE_HOME`，纯环境变量，无额外配置文件。

### 登录方式

无官方 CLI 子命令可驱动 OAuth。用户先跑 `kimi` TUI 登录；`subswap login kimi` = `FileBlobProvider::import_active` 导入当前凭证，不发 OAuth。`--email` / `--sso` / `--device-auth` 一律不支持。

### 删除（rm 连带原生登出）

删的是当前 live 文件所属账号时一并删除 live 文件（持 `.subswap.lock`），否则只清本地。

---

## OpenCode Console official sign-in and Go monitoring

Requirement (2026-09-29, corrected after live review): `opencode` represents only official Console login accounts. Go API keys appear under a separate `OpenCode API Key` provider and login entry; they remain available for manual import, selection, and quota monitoring, but never participate in automatic switching. Automatic OpenCode switching considers only official Console accounts and must not choose a Go key as a target. `subswap login opencode` recognizes an account already signed in through the official CLI and, when sign-in is needed, delegates to `opencode auth login opencode` on V2 or `opencode console login` on V1. After sign-in, the user selects an OpenCode Go model with `/models`; subswap monitors that Console account and workspace. Never attribute a Go key's usage to an unrelated Console OAuth account, and show unavailable usage as unknown rather than zero.

V2 API-key activation must switch the corresponding `opencode-go` credential through the official client database/command; changing only legacy `auth.json` does not select a V2 credential. V1 keeps the file-based path. A key not yet connected in V2 must be connected with the official client before subswap can select it; do not report a successful switch after changing only the legacy file. OpenCode V2 isolation is unsupported until a private database can be initialized and the selected Key verified there; reject `run`/`shell`/`env` for V2 rather than silently launching with the wrong credential.

Console quota requests must use the server URL attached to that official credential (the test-only base override is separate). Never send an enterprise or self-hosted Console token to the public default host merely because the client omitted host routing.

Confirmed on the local V2.0.16 client: OpenCode Console (`opencode`) and OpenCode Go (`opencode-go`) are separate credentials. V2 stores credentials in its SQLite database and exposes account switching through `opencode auth switch`; the legacy `auth.json` may still contain a Go key, but changing that file alone does not select a V2 credential. The Console OAuth token receives HTTP 401 from `/zen/go/v1/usage`; the Go key receives HTTP 200. The same OAuth token receives HTTP 200 from Console `/api/go/status` and `/api/billing/status` with its organization header. These Console routes are currently undocumented and require tolerant parsing and explicit error states.

### 设计（2026-09-29 落地）

`opencode` 是 Console 官方账号 Provider，`opencode-api-key` 是 Go Key Provider。两者在 registry、额度缓存和自动换号策略中完全分开；旧版混存的 Key 在启动时迁移到后者，保留账号 ID 与凭证，设置 `manual_only`。Console 账号只保存元数据，V2 Go Key 从官方数据库同步并由官方命令选中；V1 Go Key 使用文件引擎。若官方当前没有激活 Key，迁移前留下的 Key active 标记也必须清掉，不能把 registry 旧状态显示成官方当前凭证。

| 项 | Go Key（`opencode-api-key`） | Console 账号（`opencode`） |
|---|---|---|
| 身份 | `go-` + key 指纹；label `sk-…末4` | `console-` + accountID + orgID；label 邮箱或 org 名，同一 workspace 不同用户不能合并 |
| 凭证存放 | CredentialStore 存 `opencode-go` blob（现有） | **不存 secret**；额度每次从 live `opencode.db` 只读 token（官方客户端负责刷新，subswap 绝不刷 Console active/parked） |
| 发现 | V2 `opencode.db` 的 `opencode-go` 凭证；V1 `auth.json` 的 `opencode-go` 项 | V2 `opencode.db` 的全部 `opencode` 凭证（`active=1` 为当前）；V1 `account` + `account_state` 仅可识别当前账号 |
| 切换 | V2 `opencode auth switch opencode-go <credential-id>`，只接受官方已连接的 Key；V1 原子覆盖 `auth.json` 的 `opencode-go` 项 | V2 `opencode auth switch opencode <credential-id>`；V1 无非交互切号命令，先在官方客户端选账号再同步 |
| 额度 | `GET {zen/go/v1}/usage`（Bearer Go key，现有） | `GET <该账号凭证的 server>/api/go/status`（Bearer Console token + `x-org-id: <orgID>`，不跨站重定向）；`access.meters.fiveHour/week/month` 的 `usedMicroCents/limitMicroCents`（字符串微分）→ 已用% = `100*used/limit`，映射 5h/7d/mo；缺失窗口直接跳过（未知≠0）；401/403 → `needs re-login`；429 → 瞬态失败（走统一失败退避，绝不当死 key） |
| 登录 | `subswap login opencode-api-key` 导入已连接 Key，或 `-- <key>` 导入；V2 选中前须已在官方客户端连接该 Key | `subswap login opencode`：已登录则导入，未登录则委托官方登录（V2 `opencode auth login opencode` / V1 `opencode console login`） |
| 隔离运行 | 仅 V1 支持（`XDG_DATA_HOME` + `OPENCODE_AUTH_CONTENT`）；V2 拒绝 | **不支持** `run/shell/env`（凭证在 SQLite + 官方服务，无 env 覆盖机制；同 Cursor 边界），明确报错 |

- 同一订阅的两面：实测同一 workspace 下 Go key 的 `/zen/go/v1/usage` 百分比与 Console token 的 `/go/status` meters 换算一致（如月 50%），互为印证；但两者 token 不通用，查询路径必须按账号种类走对端点。
- `rm` 连带断开官方：V2 官方库是多凭证权威源，`sync_accounts` 每次都会把库里全部 Key/Console 账号导回 registry，只清 registry 的 `rm` 对 V2 必然复活。因此 `opencode` / `opencode-api-key` 的 `rm` 先经官方 `opencode auth logout <integration> <credential-id>` 断开对应凭证，再清 registry + keyring；官方已无此凭证时直接清本地（幂等）。V1 `opencode-api-key` 删的是当前 live key 时清除 `auth.json` 的 `opencode-go` 项（保留其他供应商）；V1 Console 无可验证的官方登出命令，保持只清本地。官方登出失败时 `rm` 直接报错退出、不动本地记录，避免“删了又回来”的假成功。
- SQLite 只读：`file:<db>?mode=ro` 经 rusqlite；测试隔离走既有 `SUBSWAP_OPENCODE_HOME`（fixture 目录无 `opencode.db` 即无 Console 账号，Go 单测不受影响）。
- 阻塞规则：DB 读与官方子进程调用一律 `spawn_blocking`；`opencode` 二进制缺失 → 明确报错不静默跳过。
- 默认入口和 daemon 各自同步 Console 与 Key；V2 Console 停用账号也可从官方数据库查询其账号余量。Console 无需 capture（不存 secret）；Go Key 无 refresh，所有 Key 固定 `manual_only`。

## OpenCode API Key

共享引擎第三个文件型 provider：`crates/providers/opencode/`。
官方 `~/.local/share/opencode/auth.json` 是多供应商 map；subswap **只抽出/覆盖 `opencode-go`**，其余原样保留。引擎 hook：`extract_blob` / `compose_live`（默认整文件覆盖；Codex/Kimi 不覆盖）。

Go 订阅 = API key（`{"type":"api","key":"sk-..."}`），无 refresh，不刷新。

### 本地凭证路径

| 项 | 值 |
|---|---|
| 工作目录 | `SUBSWAP_OPENCODE_HOME` > `XDG_DATA_HOME/opencode` > `~/.local/share/opencode`（Windows `%LOCALAPPDATA%/opencode`）。官方 xdg-basedir，**macOS 也是 `~/.local/share`，不是 Application Support** |
| 当前激活凭证 | V2 `<home>/opencode.db` 的 `opencode-go` 凭证；V1 `<home>/auth.json` |
| live 键 | V1 `opencode-go`；V2 `integration_id='opencode-go'` |

### 主键与展示名

- `primary_id` / `dedup_key` = `go-` + API key SHA-256 前 16 hex
- `label` = `sk-…` + 末 4 位
- store 只存 `opencode-go` 那一项 JSON

### Usage 端点与窗口映射

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `{SUBSWAP_OPENCODE_GO_BASE:-https://opencode.ai/zen/go/v1}/usage` |

- `Authorization: Bearer <api_key>`；`User-Agent: subswap/<version>`
- 响应形如 `usage.rolling` / `weekly` / `monthly`，各含 `status` / `percent` / `resetsAt`
- `percent` 已用 0~100 → `Quota.used`，`limit` 固定 100
- 窗口：`rolling` → 5h，`weekly` → 7d，`monthly` → 月
- `status: "rate-limited"` → 该窗口 100% 已用
- `401` / `403` = key 无效或无有效 Go 订阅（需重新导入）；**不得**把 `429` 当 key 作废
- 自动换号：禁用。`rolling` / weekly / monthly 只用于显示余量；Key 不成为候选，也不会被自动切走
- 测试：`SUBSWAP_OPENCODE_GO_BASE` → mock

### 隔离运行

官方无 `OPENCODE_HOME`。隔离：

1. `XDG_DATA_HOME` → 私有目录，合成 `auth.json` 写到 `<env>/opencode/auth.json`
2. V1 设置 `OPENCODE_AUTH_CONTENT` = 同一份 JSON；V2 不认此变量，也不会把私有 `auth.json` 自动选为当前 Key，所以入口明确拒绝

### 登录方式

- `subswap login opencode-api-key`：V2 从官方数据库导入已连接的 Go Key；V1 从 live `auth.json` 导入
- `subswap login opencode-api-key -- sk-...`：显式导入；V2 需先在官方客户端连接该 Key 才能选中，V1 合并写回 live
- `--email` / `--sso` / `--device-auth` 不支持

### 开源圈「号池」≠ subswap 切号

改 OpenCode 自动换号前先分清，否则会把「请求途中换 key」误做成「改本地登录文件」。

**A. 登录文件切换器**（与 subswap 同类）

| 项目 | 做法 | 不照搬 |
|---|---|---|
| `srmdn/opcode-switch`（→`opcode-kit`） | 每号快照，整份覆盖登录文件 | 整文件覆盖抹掉其它供应商；subswap 只改 `opencode-go` |
| `@ceritahmt/opencode-as` | 按供应商 profile；见「用量上限」文案后可选自动切 | 靠错误文案非额度接口；切完常需重开客户端 |
| `farion1231/cc-switch` | 桌面端管 `auth.json` 多供应商配置 | 管供应商配置，不是 Go 号池 |

**B. 请求途中号池**（会话不换登录文件，限流当场换 key）

| 项目 | 做法 | 要点 |
|---|---|---|
| `dhaalves/opencode-swap`（`oswap`） | 本机代理挡 Go 接口，限流换 key 再试 | 插件钩子拦不到限流故走代理；`Retry-After` 有时是周重置日期，冷却上限封 1h |
| `masrurimz/opencode-go-multi-auth` | 插件粘滞 key，限流再换 | **不做提前查额度**；号池在插件配置，不改官方登录文件 |
| `Rishabh-Bajpai/opencode-go-multi-auth` | 插件 + 本机路由，402/429 冷却换 key | 不预测额度；改官方接口地址指向本机 |
| `bytesifter/opencode-round-robin` | 随机抽 key；限流只冷却、**当次不重发** | 「请求太快」与「额度用尽」分两种冷却 |
| `rahadiana/opencode-multi-account` | 多供应商号池，限流按优先级切 | 会回写登录文件对应项；钩子常看不到 429 |

官方 OAuth 多账号轮换**不覆盖 Go 纯 API key**。

**subswap 边界**：Go Key 只查余量并允许手动选择，自动换号仅属于官方 Console 账号。subswap 不拦截或重发 OpenCode 当前请求；请求途中换 Key 需插件或代理。V2 的当前 Key 来自官方数据库，不能靠覆盖 `auth.json` 假装切换成功。旧文档「Go 无用量接口」已过时（有 `GET …/zen/go/v1/usage`）。

**切号后旧 OpenCode 会话可能固定 400**：手动换 Go Key 会改变上游眼中的 caller/key，旧会话 transcript 里持久化的 `reasoning.encrypted_content`（caller-bound）再 replay 会被拒（`encrypted_content was not issued to this caller`），重试必现。切号后开新会话（`/new`）或先 `/clear`；不要把该 400 当额度耗尽继续切号。详见 [troubleshooting/2026-09-29](troubleshooting/2026-09-29-opencode-reasoning-encrypted-content-caller.md)。

---

## Command Code

共享引擎第四个文件型 provider：`crates/providers/commandcode/`。
官方 `~/.commandcode/auth.json` 只服务 Command Code；subswap 导入时归一成 `{"type":"api","key":...}`，激活写回 `{"apiKey":...}`（官方 CLI / 第三方都能读）。无 refresh，不刷新。

### 本地凭证路径

| 项 | 值 |
|---|---|
| 工作目录 | `SUBSWAP_COMMANDCODE_HOME` > `~/.commandcode` |
| 当前激活凭证 | `<home>/auth.json` |
| live 形态 | 优先 `apiKey`；也认 `commandcode` / `command-code` 槽或 OAuth `access` |

### 主键与展示名

- `primary_id` / `dedup_key` = `cc-` + API key SHA-256 前 16 hex
- `label` = `cc-…` + 末 4 位
- store 存归一化条目 JSON

### Usage 端点与窗口映射

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `{SUBSWAP_COMMANDCODE_BASE:-https://api.commandcode.ai}/alpha/billing/credits` |

- `Authorization: Bearer <api_key>`；`User-Agent: subswap/<version>`
- **undocumented `/alpha/*`**：官方 CLI `/usage` 在用，但未进公开 Provider API 契约；解析必须宽容。禁止改走需 Web Cookie 的 `/internal/usage`
- 响应：
  - `windowLimits.fiveHour` / `weekly`：`used`/`cap` 为美元 → 已用% = `used/cap*100`（`exceeded:true` → 100%）；`resetAt` 为 epoch **毫秒**
  - `credits.monthlyCredits` + `purchasedCredits` + `freeCredits`：余量美元之和 → `QuotaWindow::Credits`（存分）；未知套餐总额时有余量 `used=0/limit=remaining_cents`，耗尽 `1/1`
- 窗口：`fiveHour` → 5h，`weekly` → 7d，credits → `$`
- `401` / `403` = key 无效（需重新导入）；**不得**把 `429` 当 key 作废
- 自动换号：5h 过默认阈值切走；7d / Credits 只在明确耗尽时触发。Credits **不**进并行池（并行语义由 provider 声明，Command Code 声明为叠加）。
- 测试：`SUBSWAP_COMMANDCODE_BASE` → mock

### 隔离运行

官方 CLI 用 `homedir()/.commandcode/auth.json`。隔离：

1. `HOME` → 私有目录，合成 `auth.json` 写到 `<env>/.commandcode/auth.json`
2. `COMMANDCODE_API_KEY` = 同一把 key（给只读环境变量的工具）

### 登录方式

- `subswap login commandcode`：从 live `auth.json` 导入（用户先 `command-code login`）
- `subswap login commandcode -- <api-key>`：直接导入并写回 live
- 别名：`command-code` / `cmd`
- `--email` / `--sso` / `--device-auth` 不支持

### 删除（rm 连带原生登出）

删的是当前 live 文件所属账号时一并删除 live 文件（持 `.subswap.lock`），否则只清本地。

---

## Cursor

非文件型 JSON Provider：登录在本地客户端存储，切换可能协调 GUI 退出/重启。独立 `Provider`，不接 `crates/providers/common`，不支持 `subswap run/shell/env` 隔离。

`CredentialSource`（`crates/providers/cursor/src/lib.rs::CredentialSource`）统一抽象两种客户端：

- **桌面版（Electron）**：SQLite `state.vscdb` 的 `ItemTable`
- **命令行 agent（`cursor-agent`）**：元数据 `~/.cursor/cli-config.json` 的 `authInfo`；token 随平台/开关变化：
  - macOS 默认钥匙串（service `cursor-access-token` / `cursor-refresh-token`，account `cursor-user`），不落盘
  - 文件后端：macOS `~/.cursor/auth.json`，Linux `~/.config/cursor/auth.json`（或 `$XDG_CONFIG_HOME/cursor/auth.json`）

探测：显式指定桌面库 → 桌面；否则桌面能读出有效登录 → 桌面；桌面仅未登录遗留且命令行已登录（钥匙串或 `auth.json` 有 access）→ 命令行；皆无 → 回退桌面路径并提示先登录。

macOS 命令行钥匙串**只能 fork `/usr/bin/security`**，禁止 `keyring` crate（会把 ACL 改成「仅 subswap」，`cursor-agent` 反复弹授权；见 [troubleshooting/2026-06-11](troubleshooting/2026-06-11-claude-code-keychain-acl-poisoning.md)）。
**已有条目只更新密码、禁止 delete 再 add**（删建收成「仅 security」→ 桌面邮箱对、请求报未登录）。新建才把 `/usr/bin/security` 与 Cursor.app 写入信任名单。漏读钥匙串故障：[troubleshooting/2026-08-14](troubleshooting/2026-08-14-cursor-quota-missing-cli-keychain.md)。

**live 归属以令牌 JWT 为准，禁止拼到过期身份。** 匹配主人只认 access JWT `sub`；`authInfo.authId` 与 JWT 不一致 → 忽略该身份，回灌不得用过期邮箱盖主人；JWT 对不上该账号 → `needs re-login`，禁止查询/刷新。[troubleshooting/2026-08-14](troubleshooting/2026-08-14-cursor-quota-cloned-across-accounts.md)。

### 本地状态与跨平台路径

桌面版 `state.vscdb`：

| 平台 | 默认路径 |
|---|---|
| macOS | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` |
| Linux | `~/.config/Cursor/User/globalStorage/state.vscdb` |
| Windows | `%APPDATA%\Cursor\User\globalStorage/state.vscdb` |

命令行：元数据 `~/.cursor/cli-config.json`；token macOS 默认钥匙串，文件后端见上。macOS 优先读钥匙串。

测试重定向：`SUBSWAP_CURSOR_STATE_DB_PATH`（桌面）；`SUBSWAP_CURSOR_AGENT_AUTH_PATH` / `SUBSWAP_CURSOR_AGENT_CONFIG_PATH`（agent 文件）；`SUBSWAP_CURSOR_KEYCHAIN_PATH`（一次性 keychain）。相对路径直接报错；隔离契约见 [OPERATIONS_GUIDE.md](OPERATIONS_GUIDE.md)「三平台测试隔离」。

桌面只读写 `ItemTable` 身份键：`cursorAuth/accessToken`、`cursorAuth/refreshToken`、`cursorAuth/cachedEmail`、`cursorAuth/authId`，并同步 `cursor.accessToken` / `cursor.email`；其余不动。agent 写回：令牌与 `authInfo` 成套；文件后端写 `accessToken` / `refreshToken`，钥匙串写对应条目，同步邮箱/authId；保留其它字段。CredentialStore 存同构私有 JSON blob；registry 只存邮箱、稳定身份与展示元数据。

### 登录、导入与切换事务

`subswap login cursor` 不复制 OAuth、不驱动网页：用户先在客户端登录（桌面或 `cursor-agent login`），命令只读本地凭证导入/覆盖并标 active。默认入口同步当前 live（同 Claude/Codex/Kimi）。

**删除（rm 连带原生登出）**：删的是当前 live 账号时一并从客户端登出——桌面版先请 Cursor 正常退出（超时则报错不清本地），再在 SQLite 事务里清除身份键，**不再拉起**；agent 文件后端清除 `auth.json` 令牌 + `cli-config.json` 的 `authInfo`（保留其他字段），钥匙串后端删除对应 item 再清 `authInfo`。parked 删除只清本地。登出失败则报错退出、不清本地。**无墓碑**（墓碑曾致无声消失，已移除；[troubleshooting/2026-08-15](troubleshooting/2026-08-15-cursor-section-silently-missing.md)）——但 rm 已连 live 一起清，要重新出现需在客户端重新登录后再 `login`。

**新登录优先入池（产品约束，2026-09-06）**：Cursor 只有一份 live 凭证。无论是默认入口还是 daemon 先观察到一个未登记的 live 账号，都必须先将它导入并标为 active，之后才能建立额度快照或执行自动切换。导入失败时该轮必须跳过 Cursor 自动切换，绝不能让旧账号池覆盖这份新凭证；成功导入后即使该账号额度耗尽而被自动切走，它也必须保留在账号池中。此约束防止 `agent login` 成功后新账号尚未显示便被切回旧号。

**agent 切换**：capture-on-leave → 快照旧令牌/`cli-config.json`/registry → 写回目标令牌 + `authInfo` → 标 active；失败三者回滚。无进程协调。

**桌面版**进程存活时不能直接改 SQLite（退出阶段可能用内存旧 token 盖回）：

1. 检测运行 → 请求正常退出并等完全结束，超时则不切换
2. 读 live + capture-on-leave；live 缺 refresh **绝不**覆盖仓库有 refresh 的副本
3. 快照六个身份键，SQLite transaction 写目标 blob → 标 registry active
4. 切换前若在运行 → 成功后重开并确认启动
5. 任一步失败 → 恢复数据库与 registry；原在运行则重开旧会话

退出：macOS 系统退出事件，Linux TERM，Windows `taskkill /IM Cursor.exe`（不强杀）；皆等退出完成。

### 额度与刷新边界

| 用途 | 方法 | URL |
|---|---|---|
| 用量（1st / API） | GET | `https://cursor.com/api/usage-summary` |
| **Credits 赠送余额** | POST | `https://cursor.com/api/dashboard/get-credit-grants-balance`（body `{}`） |
| parked token 刷新 | POST | `https://api2.cursor.sh/oauth/token` |

用量与 Credits **同一套** WorkOS session cookie（从 access subject 生成），不用 Bearer。

### `usage-summary`（1st / API）

解析 `individualUsage.plan`（兼容 snake_case / `planUsage`）：

- `autoPercentUsed` → 标签 `1st`
- `apiPercentUsed` → `API`
- `billingCycleEnd` → `1st` / `API` / Credits 共用 reset（Credits 接口无周期时沿用）

`plan.used` / `limit` / `remaining`（常为 `2000` 分 = $20）与 **`API` 同一套餐已含额度**，**不是** Spending 页 Credits。禁止映射成 `QuotaWindow::Credits`。

### Credits（赠送额度，Spending 页）

权威：`POST /api/dashboard/get-credit-grants-balance`，形如：

```json
{ "hasCreditGrants": true, "creditBalanceCents": "1110", "totalCents": "2500", "usedCents": "1390" }
```

- 字段常为**字符串**分；`creditBalanceCents` = 剩余，`usedCents`/`totalCents` = 已用/上限
- `hasCreditGrants != true` 或 `{}` → **不展示** Credits（缺 `$` 列 = 无赠送，不是漏查）。禁止硬画 `$0.00`。有 `$` 列且余量 0 = 有过赠送已用尽
- 写入 `QuotaWindow::Credits`：`used`/`limit` 存分；CLI 标签 `$`（如 `$11.10 left`），排在 `1st`/`API` 之后
- 【裁定 · 2026-09-05】Credits ≠ Pro 的 $20 API 已含池。曾误用 usage-summary `used`/`limit` 当 Credits → 全员 `$0.00` 误导换号。见 [troubleshooting/2026-09-05](troubleshooting/2026-09-05-cursor-credits-zero-despite-claimed-remaining.md)
- Credits 接口返回 `{}` → 无 `$` 列属正常（三池皆空亦可）

### 自动切换

- **并行可用池**：`1st`、**Credits**、**API**——任一池 `Ok`/`Warn` 即可作候选；仅全部 `Exhausted` 才触发/阻断（异于 Claude 5h+7d 叠加）
- 【裁定 · 2026-09-05】全员 `1st` 0%、仅某号 `API` 有余量 → 必须切到该号；禁止按重置时间优先挑全空号。[troubleshooting/2026-09-05](troubleshooting/2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md)
- `1st` 有余量时，不要只因 `API` 耗尽就切走（2026-08-21）
- Credits 耗尽：`creditBalanceCents == 0`（或 `used >= total`）；不走小时级 `threshold` 提前切
- 细则：[AUTO_SWAP_DESIGN.md](design/AUTO_SWAP_DESIGN.md) §1.1；[2026-08-21](troubleshooting/2026-08-21-cursor-auto-swap-to-zero-over-remaining.md)

active 查询 401 **绝不刷新**：只重读 live。Cursor 已自行轮换 → capture 回仓库并用新 token 重试一次；否则认证错误。parked 允许在 subswap 跨进程文件锁内刷新：锁内重读，另一进程已轮换则复用；否则刷一次并持久化完整 pair。401/403 或 `shouldLogout` → refresh SHA-256 dead guard。

---

## 文件型 OAuth 切换共享引擎（`crates/providers/common`）

Codex / Kimi / OpenCode API Key：凭证以 JSON blob 进入公共 `subswap-provider-common`；OpenCode V2 的当前 Key 以官方数据库为准，V1 才按文件原子覆盖切换。

**Claude 不在引擎上**：Keychain + 自定义 API 账号形状不同，保留 `crates/providers/claude` 独立实现。

### 引擎（`FileBlobProvider<A: FileBlobRuntime>`，`engine.rs`）

引擎实现完整 `Provider`；adapter 实现 `FileBlobRuntime`：

| 机制 | 说明 |
|---|---|
| `activate` 原子切换 | flock → snapshot → 原子写（tmp+rename+0600）→ 失败回滚 |
| capture-on-leave | 覆盖前 live→所属账号 store；缺 refresh 快照不覆盖有 refresh 副本 |
| capture-on-arrival | `reconcile_active_from_live`：只 live→store，不刷新、不写 live |
| parked-only 刷新 | `query_quota` 只对 parked 调 `runtime.refresh()`；active 只读不刷 |
| blob fallback | `raw_blob_for_account`：active 优先 live（顺手修 store）→ store → `recover_legacy`；store 失败先试 legacy |
| 隔离 | `export_blob` / `absorb_blob` → `IsolatedProvider`（`isolated.rs`）；blanket impl 自动获得 |
| 导入 | `import_active` / `sync_active_metadata`（只对齐 active 标记）/ `import_raw` / `import_raw_with_explicit_metadata` |
| 删除 | `disconnect_live_file_if_matches`：删的是 live 文件所属账号时持 `.subswap.lock` 删除 live 文件，否则只清本地（`rm` 先调它，失败则不清本地） |

### Adapter（`FileBlobRuntime`，`runtime.rs`）差异点

| 方法 | 用途 | Kimi | Codex（迁移前存量，需覆盖默认） |
|---|---|---|---|
| `id()` / `display_name()` | 标识 / 展示 | `"kimi"` / "Kimi / Moonshot" | `"codex"` / "Codex / ChatGPT" |
| `home()` | 工作目录 | `KIMI_CODE_HOME` 等 | `CODEX_HOME` 等 |
| `live_cred_path()` | live 路径 | `<home>/credentials/kimi-code.json` | `<home>/auth.json` |
| `parse_metadata()` | 抽 `BlobMetadata` | JWT `user_id` | `account_key`/`email`/`chatgpt_account_id` 等 |
| `refresh()` | → `RefreshOutcome` | 真刷（`oauth.rs`） | `Unsupported`（官方刷） |
| `fetch_quota()` | 查额度 | `GET /usages` | `openai_usage` + legacy 缓存 |
| `isolation()` | 隔离 env + 原生 CLI | `KIMI_CODE_HOME` / `kimi` | `CODEX_HOME` / `codex` |
| `extract_blob()` / `compose_live()`（可选） | 多供应商共存只抽/写本项 | 默认整文件 | 默认整文件 |
| `access_token()`（可选） | 抽额度 token | 默认找 `access_token` | 默认 |
| `isolation_rel_path()` / `isolation_extra_env()`（可选） | 隔离相对路径/额外 env | 默认 | 默认 |
| `store_field()`（可选） | store 字段名 | 默认 `"blob"` | `"auth_json"`（存量兼容） |
| `dedup_extra_key()`（可选） | registry extra 去重键名 | 默认（无需求） | `"chatgpt_account_id"`（存量兼容） |
| `recover_legacy()`（可选） | store/live 皆无时恢复 | 未用 | `~/.codex/accounts/...`（`legacy.rs`） |
| `materialize_extra()`（可选） | 隔离物化额外动作 | 未用 | 复制 `~/.codex/config.toml`（`legacy.rs::copy_codex_config_best_effort`） |

**新增无存量文件型 provider**：实现前 8 个必填 + `isolation()`；`store_field()` / `dedup_extra_key()` 保持默认即可（Codex 两行覆盖仅为存量免重导入）。

<!-- 该文档整理/压缩于 2026-09-05 -->

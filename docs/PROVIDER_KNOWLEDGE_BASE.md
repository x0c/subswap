# Provider 知识库

记录各 Provider 的上游接口、本地文件、认证字段等代码不能表达的事实。

> 新加 Provider 时按本文档结构补一节。

---

## 额度语义（跨 Provider 统一约定，先读这条）

**数据层与状态层一律用「已用百分比」，只有 CLI 展示层转成「余量」。** 别把两者搞混。

| 层 | 语义 | 位置 |
|---|---|---|
| 上游字段 | **已用 %**（0~100） | Claude `utilization`（`oauth.rs::WindowUsage`）；Codex `used_percent` / `percent`（`openai_usage.rs`，注释原文「已用百分比」） |
| `Quota` 模型 | **已用**：`used`(0~100) + `limit`(固定 100) | `make_quota`（claude）/ `query_quota`（codex）都把已用% 写进 `Quota.used` |
| 状态判定 | 基于**已用%**：`used ≥ quota.warn_pct`(默认 90)→Warn，`≥ quota.exhausted_pct`(默认 100)→Exhausted | `QuotaStatus::from_percent` |
| CLI 展示 | 所有 Provider 统一显示**余量** `{limit - used}% left`（含 Cursor 的 `1st` / `API`） | `render.rs::format_quota_compact` |

记忆点：**所有 Provider 的数据语义都是已用**，CLI 展示层统一翻成余量。Cursor 上游字段仍是
`autoPercentUsed` / `apiPercentUsed`，写入 `Quota.used` 后展示时再算 `{100 - used}% left`，与
Claude/Codex/Kimi 口径一致。改展示格式时**不要翻转 `Quota.used` 的语义**。

**既定 UX 约定（勿改回）**：CLI 默认统一显示**余量** `{N}% left`，不打印 ok/warn/full 文字；
严重程度由数字 + 颜色传达，状态文字块冗余。

---

## Claude / Anthropic

### OAuth 公开常量

| 项 | 值 | 备注 |
|---|---|---|
| Client ID（默认） | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` | 公开值，非 secret |
| 环境变量覆盖 | `SUBSWAP_CLAUDE_OAUTH_CLIENT_ID` | 留作上游变更时的逃生口 |
| 实现位置 | `crates/providers/claude/src/oauth.rs::DEFAULT_CLIENT_ID` | |

### 上游 HTTP 端点

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `https://api.anthropic.com/api/oauth/usage` |
| Token 刷新 | POST | `https://platform.claude.com/v1/oauth/token` |

公共请求头：
- `Authorization: Bearer <access_token>`（usage）
- `anthropic-beta: oauth-2025-04-20`（usage；上游调整须同步常量 `BETA_HEADER`）
- `User-Agent: subswap/<version>`

Token 刷新请求体：

```json
{"grant_type":"refresh_token","refresh_token":"...","client_id":"..."}
```

### Usage 响应字段（subswap 关心的）

- `five_hour.utilization` — 0~100 百分比
- `five_hour.resets_at` — ISO8601
- `seven_day.utilization` / `seven_day.resets_at`
- `extra_usage.utilization` / `extra_usage.resets_at` / `extra_usage.monthly_limit` / `extra_usage.used_credits`

`utilization` 固定按 0~100 的已用百分比解析。小于 1 的值仍表示不到 1% 已用，不能当成 0~1 比例放大，
否则会把 `0.97%` 错误解析为 `97%`。

**这是未公开接口，字段类型会在 Claude Code 版本间漂移，必须逐字段宽容解析。**
`oauth.rs::lenient` 让任一字段解不出时只退化成 `None`，绝不能让一个字段把整份响应解崩。
`Option<T>` 只容忍「字段缺失」，容忍不了「类型变化」——别再指望它兜底。
已知漂移：2026-07 `extra_usage.used_credits` 从整数变成小数（伴随新增 `currency` /
`decimal_places`，金额已是小数语义，故 `monthly_limit` / `used_credits` 一律用 `f64`）；
同期 `extra_usage` 不再返回 `resets_at`，并新增一批全为 null 的代号窗口（`tangelo`、
`omelette` 等）——未知字段本就被忽略，真正致命的只有**已知字段的类型变化**。
实测响应全文与判别手法见
[troubleshooting/2026-07-26](troubleshooting/2026-07-26-claude-usage-schema-drift-bad-response.md)。

### Usage 接口异常状态码的真实含义（429 ≠ token 失效，别再误判）

> 2026-06-14 修正：旧版本这里写「429 是鉴权失败的伪装」，是**误判**。实测拿一个**确认有效**的
> token（Claude Code 维护的 active 账号）打 `/api/oauth/usage`，**间隔 4 秒**仍是 `200 → 429 → 429`，
> 且带 `retry-after`。所以 **429 是这个端点真实的、极严的限流**，不是 token 问题。token 失效的真实
> 信号在别处（见下）。完整排查见
> [troubleshooting/2026-06-14](troubleshooting/2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md)。

**三种「查不出余量」要分清，处理路径完全不同：**

| 信号 | 端点/状态 | 含义 | 处理 |
|---|---|---|---|
| `429 rate_limit_error` | usage `/api/oauth/usage` 429 + `retry-after` | usage 端点限流极严，约**每账号每分钟才放 1 次** | 缓存节流(见下)，**不是**重登 |
| `invalid_grant` | refresh `/v1/oauth/token` 400 | parked 账号存的 refresh token 已死 | 死 token 守卫 + 重登(见「Refresh token 轮换」) |
| `401` | usage 401 | active 账号 live token 过期、Claude Code 未刷 | 开一次 Claude Code 让它刷；subswap 不刷 active，失败退避最多保留 90s |
| `bad response` | usage **200** 但 parse 失败 | 上游响应结构漂移（见「Usage 响应字段」） | 补宽容解析；**能走到 parse 就说明鉴权和限流都没问题** |
| 空 access token | 本地凭据 | Claude Code 登录/切换的中间态被错误回灌，账号副本已不完整 | 不发 usage 请求，直接显示 `needs re-login`；回灌时保留 store 完整副本 |

**缓存节流（治 429，`crates/cli/src/cmd/default.rs` + `crates/daemon/src/unix.rs::build_snapshots`）**：
subswap 每次 CLI 运行 + daemon 每轮都把所有账号一起查，极易并发打爆 usage 端点 → 全员 429。
对策：daemon 与 CLI **共用** `quota_cache.json`，查询前先看缓存——`QuotaCache::fresh()` 判定缓存
比 `settings.quota.min_refresh_interval_ms`(默认 90s，> daemon 60s 轮询) 新就**直接复用、不打端点**。
谁先查到谁刷新 `cached_at`，另一方据此跳过，把每账号请求频率稳定压到 ~90s 一次。

**失败退避（治「坏账号反噬」，`QuotaCache::record_failure` / `in_failure_backoff`）**：
上面的缓存节流**只覆盖成功路径**——失败结果不写 `entries`，等于失败路径完全没有节流，
一个必然查不出的账号会被 daemon 每轮（60s）重打，频率反而高于健康账号，把限流桶打空后
429 蔓延到同账户下的其他账号。对策：失败单独记在 `failures` 表，按
`min_refresh × 2^(连续失败次数-1)`（封顶 `settings.quota.failure_backoff_max_ms`，默认 15 分钟）
退避，查成功一次即清零。**401/403 鉴权失败例外封顶为一个 `min_refresh`（默认 90 秒）**：原生客户端
可能在失败后立刻刷新凭据，若仍保留 15 分钟旧失败，会出现「Claude 已能正常对话、subswap 仍报旧 401」。
这不是同一次查询里的盲目重试，仍至少间隔 90 秒。**新增任何 quota 查询路径都必须同时接失败退避**，
只接成功缓存是不够的。

- **排查雷区**：别手动 `curl` usage 端点连发几次去"复现"——会自己把限流桶打空、污染判断（我就踩了）。
- subswap 不把 429 翻译成 401：`oauth.rs::fetch_usage` 保留原始状态码；CLI 压成 `429 rate limited`
  短文案，stale 行会显示 `(cached ~Xm · 429 rate limited)`，排查看 `--log debug` 原始 message。

### 本地激活文件

| 路径 | 用途 |
|---|---|
| `~/.claude/.credentials.json` | OAuth 凭证；Claude CLI 实际读取 |
| `~/.claude.json` | 新版全局配置；含 `oauthAccount` 子树 |
| `~/.claude/.config.json` | 旧版全局配置；存在则优先 |

`.credentials.json` 结构（subswap 关心的字段）：

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

其他字段通过 `#[serde(flatten)]` 透传保留，避免上游加字段时丢失。

**`refreshTokenExpiresAt`（Claude Code 2026-07-09 起写入，实测约 30 天）**：refresh token 现在会
**自然过期**，不再是「只要没被轮换作废就一直能刷」。到期后再刷只会拿回 `invalid_grant`，
所以 `refresh_token_expired()` 会提前判掉、不发这次请求，并直接透出 `needs re-login`。
字段缺失（老版本客户端）时保持原有行为不判过期。**后果**：长期停泊不用的账号到期后必须去
Claude Code 重新登录，这不是 subswap 把凭证写坏了——与
[live capture 覆盖 refresh token](troubleshooting/2026-06-18-live-capture-clobbers-refresh-token.md)
是两个不同的重登原因，排查时先看该字段有没有到期再怀疑覆写。

`oauthAccount` 子树（subswap 关心的字段）：

```json
{
  "emailAddress": "...",
  "accountUuid": "...",
  "organizationUuid": "...",
  "organizationName": "..."
}
```

### 切换 (activate) 触达的文件

1. 整段重写 `~/.claude/.credentials.json`（原子，0o600）
2. 只替换 `~/.claude.json` 的 `oauthAccount` 子树（其他字段如 `projects` 必须保留）
3. 由 `fs2::FileExt::lock_exclusive` 在 `<claude_home>/.subswap.lock` 上加文件锁

切换路径上 token 预刷新是 **best-effort**：检测到 `expiresAt` 在 5 分钟内过期且
keyring 中有 `refreshToken` 时调 refresh 端点；失败仅 warn 不阻塞切换（不变量 #1）。

### Claude Code 自定义 API

Claude Code 支持在 `~/.claude/settings.json` 的 `env` 中配置兼容端点。DeepSeek 官方 Anthropic
兼容端点为 `https://api.deepseek.com/anthropic`，认证使用 `ANTHROPIC_AUTH_TOKEN`，并需要把
Claude 的 Opus / Sonnet / Haiku 三档角色映射到 DeepSeek 模型。

Kimi 官方编码端点为 `https://api.kimi.com/coding`，认证使用 `ANTHROPIC_API_KEY`（`x-api-key`
请求头）。模型按会员档位解锁、**不会自动路由**（`kimi-for-coding` 是各档位通用的 K2.7 基础款，
`k3` / `k3[1m]` 旗舰款仅 Moderato+ / Allegretto+ 可用，`kimi-for-coding-highspeed` 高速档仅 Allegretto+），
因此 `add-api` 的 Kimi 预设让用户**分别选择 Opus、Sonnet、Haiku 三档模型**。三档默认都用
`kimi-for-coding`，以免低档位账号选到用不了的模型而 400；非交互缺省也使用该值，
`--opus-model`、`--sonnet-model`、`--haiku-model` 可分别覆盖。

subswap 中自定义 API 与 OAuth 账号共用 `provider = "claude"`，但账号元数据带：

```toml
[accounts.extra]
kind = "api"
manual_only = true
```

- API Key 单独存入 `CredentialStore(field=api_key)`；registry 只存端点与模型映射。
- 激活 API 时合并写 `settings.json.env`，保留 hooks、permissions、plugins 和其他 env。
- `.subswap-api.json` 保存 active API id 与切入前受管 env 的恢复值；文件与切换快照都必须为 `0600`。
- 切回 OAuth 时恢复原受管 env 并删除标记，避免 OAuth 凭证已切回但请求仍被 API env 覆盖。
- API active 时 API Key 按 Claude Code 的要求以明文存在于 `settings.json`；这是上游配置机制的安全边界。
- API 账号 `query_quota` 返回空列表，`manual_only` 保证它只能手动切入，active 时自动换号停用。

### 账号计费方式（BillingKind，v0.3.23+）

`Account.billing()` 返回 `BillingKind`，读取 `extra["billing"]` 字段；供下游消费者（如 OpenConductor）
判断"按量花钱"的信号。新增 Provider 适配器只需在 `extra` 里如实标注，不需要 subswap-core 认识具体账号名。

| 枚举值 | `extra["billing"]` | 语义 |
|-------|-------------------|------|
| `Flat` | 缺省（不写 billing 字段） | 固定费率订阅（官方登录号），用量在套餐内不额外计费 |
| `Metered` | `"metered"` | 按量计费（接自定义 API 端点的按 token 计费上游） |
| `Unlimited` | `"unlimited"` | 不限量（公司自建网关、不限量 API Key）|

**向后兼容**：早于 v0.3.23 登记的 API 账号没有 `billing` 字段，但带有 `kind = "api"`；
`Account::billing()` 检测到 `kind = "api"` 时自动视为 `Metered`，无需手动补写。

JSON 输出（`subswap list --json`）中 `billing` 字段对应该值的序列化形式（`"flat"` / `"metered"` / `"unlimited"`）；
默认入口（`subswap` 无参）显示的每行账号摘要也包含 `billing` 字段。

**写入时机**：`subswap add-api` 交互向导提供三选一（metered / unlimited / flat），也可用
`--billing <value>` 直接指定；缺省为 `metered`（按量计费最安全的默认值）。

### Claude 自定义 API 的模型角色

`subswap add-api` 对第三方 Claude 兼容 API 只暴露三个独立角色：Opus、Sonnet、Haiku。交互向导和非交互参数
分别使用 `--opus-model`、`--sonnet-model`、`--haiku-model`，不再要求用户理解「主模型」「快速模型」或「子任务模型」。
激活时，Sonnet 同时写入 Claude Code 的默认模型字段，Haiku 同时写入子任务模型字段；这是内部兼容映射，用户的
三档选择始终是唯一配置来源。

**目前未影响 auto_policy 排序**：`auto_policy.rs` 的候选筛选和 `compare_candidates` 排序**不**使用
`BillingKind`，该字段当前只供下游消费者（如 OpenConductor）做"是否按量花钱"判断，
不改变 subswap 自身的自动切换决策。

---

## Codex / ChatGPT

### 上游 HTTP 端点

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `https://chatgpt.com/backend-api/wham/usage` |
| 账户元数据 | GET | `https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27` |

请求头：
- `Authorization: Bearer <access_token>`
- `ChatGPT-Account-Id: <chatgpt_account_id>`
- 浏览器风格 `User-Agent`（避免被识别为非交互客户端）

active 账号不再把上述兼容 HTTP 端点作为首选。subswap 先通过官方 `codex app-server` JSONL 协议调用
`account/rateLimits/read`，优先复用 `<CODEX_HOME>/app-server-control/app-server-control.sock` 控制通道；
parked 账号无法安全物化完整官方认证状态，仍走 `wham/usage` 兼容查询。

### Usage 响应字段（不稳定）

ChatGPT 后端响应字段会随产品调整；subswap 在 `openai_usage::normalize()` 里做宽松解析：

- 顶层与 `usage / quota / limits` 嵌套都尝试
- 新版 `primary / secondary` 窗口可出现在任意嵌套层级，都会递归识别
- 新版 `rate_limit.primary_window / rate_limit.secondary_window` 也会递归识别
- 候选字段：
  `used_percent / percent / used / limit / resets_at / reset_at / window_minutes / limit_window_seconds`
- 任意字段都无法解析时返回 `Quota { status: Unknown }` 而不是 `Err`
- 若实时接口成功但字段不可识别，且账号带有旧版本地 usage 缓存，subswap 可使用新鲜的
  `last_usage` 本地缓存兜底；缓存有效期见 `defaults::CODEX_USAGE_CACHE_MAX_AGE_MS`

### 本地激活文件

| 路径 | 用途 |
|---|---|
| `~/.codex/auth.json` | 当前激活账号；**Codex CLI / VSCode 扩展 / 桌面端共用同一文件** |

因此切换 = 只需要写这一个文件即可同步三端。

### Token 刷新分工

**Claude**：
- `activate` 路径会在 token 临近过期时做 best-effort 预刷新；失败仅 warn，不阻塞 `swap`。
- 非活跃账号的 `access_token` 只存在凭证仓库里，没人帮它刷，**subswap daemon (M4) 负责后台自动保活**：
  周期扫描 `expires_at`，临近过期且有 `refresh_token` 时调 Anthropic OAuth 端点 + 写回凭证仓库。
- 不暴露 `subswap refresh` 子命令；保活是应用后台职责，不进入日常用户工作流。

### Codex 官方额度通道与刷新边界

subswap **不实现 OpenAI OAuth**、不硬编码 OAuth client id，也不直接调用 token 端点。active 账号只把刷新
交给官方 app-server：

1. 控制 socket 存在时，通过 `codex app-server proxy --sock <socket>` 复用正在运行的官方认证状态，避免
   只读到磁盘上滞后的 access token。
2. 没有控制 socket、且确认没有普通 Codex 进程时，可短暂启动 `codex app-server --stdio`。先读额度；仅在
   官方返回认证失败时调用 `account/read {refreshToken:true}` 强刷一次，再重试额度一次。
3. 没有控制 socket、但普通 Codex 正在运行时，仍可启动临时 app-server 查询，不过使用 `0600` 的临时
   `CODEX_HOME`，只复制 live `auth.json` 并清空 refresh token。这样官方查询能用现有 access token，
   但绝不可能与正在运行的 Codex 抢刷或覆盖真实凭证。
4. 官方通道不可用、认证失败或当前版本不支持方法时，才回退 `wham/usage`；官方 429 与其他服务错误原样
   返回，**不能二次回退再打一条请求**，否则会放大限流。

parked 账号仍只走兼容查询：共享引擎传给额度适配器的只有 access token，若为调用官方服务临时拼一个残缺
`auth.json`，刷新后的完整 token 对无法安全吸收回账号仓库，反而会制造一次性 refresh token 分叉。

外层 `quota.fetch_timeout_ms`（默认 20s）必须盖住本会话上限：过短会把尚在跑的 app-server 查询取消成
可重试的 `quota fetch timeout`，默认入口最终显示 `timeout after N attempts` 并回落旧缓存。Kimi active
401 自愈（探测官方锁协议 + 持锁刷新）同样受该超时约束。

这解决了「Codex 对话正常但磁盘 token 滞后，subswap 查询 401」的大多数 active 场景；若官方刷新也拒绝，
才需要在 Codex 中重新登录。完整排查与方案演进见
[troubleshooting/2026-07-09](troubleshooting/2026-07-09-codex-quota-401-despite-working-cli.md)。

### Refresh token 轮换与 capture-on-leave（核心安全约束）

**两边的 refresh token 都是一次性轮换**：刷新一次旧 token 立即作废。subswap 与原生客户端
（Codex CLI / Claude Code）若各自独立持有同一份 refresh token 并各自刷新，必然有一方被服务端
作废，表现为 `refresh token already used` 强制重登（排查见
[troubleshooting/2026-06-08](troubleshooting/2026-06-08-codex-refresh-token-already-used.md)）。

**不变量：active 账号只能在原生客户端认可的协调机制内轮换。** Claude/Cursor active 账号只读不刷；
Codex 只委托官方 app-server；Kimi 只有识别出当前版本官方锁协议并成功持锁时才允许自愈。停泊（parked）
账号可由 subswap 按各 Provider 的串行化边界刷新/恢复，Cursor 明确使用跨进程文件锁。落地机制：

1. **Capture-on-leave**：`Provider::activate` 在覆盖 live 文件前，先读当前 live 凭证、找受管
   owner 账号、回写其 store（Codex/Kimi 走共享引擎，Claude/Cursor 各自实现）。否则切走的账号
   store 副本会停在旧 token，下次切回写回旧 token → 作废。所有 swap（手动 + daemon 自动）唯一
   经过 `activate`，一处生效覆盖两条路径；找不到 owner 直接跳过（best-effort，不阻塞 swap）。
   Claude 重复切换当前账号时只执行回灌并直接返回，禁止先读 store 再把陈旧 token 覆盖回 live。
   - **覆盖前必须比较新旧 access / refresh token，绝不能用缺字段的快照覆盖对应字段完整的快照**。
     原生客户端轮换 token 期间 live 源可能短暂处于不完整状态；这一刻被回灌捕获到会把 store 里
     可续期的副本永久写死；任何 access token 自愈都不能凭空补回丢失的 refresh token（排查见
     [troubleshooting/2026-06-18](troubleshooting/2026-06-18-live-capture-clobbers-refresh-token.md)）。
     Claude 缺 refresh 时合并保留旧 refresh、只跟进非空的新 access；Claude 缺 access 时整段保留
     store 完整副本；Codex 命中时整段跳过本次回灌
     （遵循下方「opaque blob」处理原则，不做字段级合并）。
2. **Claude active 账号绝不轮换 token**：
   - `refresh_if_near_expiry` 开头加 active 守卫（`active_account_id()` 命中即返回 `Ok(false)`），
     daemon 后台保活只对 parked 账号生效。
   - `query_quota` 401 自愈仅当凭证来自 store（parked）才刷新；来自 live（active）直接返回错误，
     交还 Claude Code 自刷。
   - macOS 的 active 凭证读取必须优先 Claude Code Keychain；`.credentials.json` 只是兼容副本，
     不能用它覆盖或查询当前账号。

一般 quota 查询遇到 `401` / `403` / `429` 时不盲目重试；仅 Codex 官方 app-server 与持有官方锁的
Kimi active 恢复、以及 Cursor active 重读到更新后的 live access token，允许各重试一次。429 永不重试或
切换通道，否则会延长 `quota loading` 并加重服务端限流。

**capture-on-leave 的残留缺口 + 两道补救（2026-06-14）**：capture-on-leave 只在**经 `subswap swap`
切走**时触发。若用户**直接在 Claude Code 里登录/切换**某账号（绕过 subswap），Claude Code 在钥匙串里
把该账号 token 轮换掉，而 subswap store 里那份变陈旧；等它变 parked、daemon 去保活刷新 → 服务端回
`invalid_grant`（refresh token not found or invalid），daemon 每轮拿同一死 token 反复刷成请求风暴。两道补救：

1. **死 token 守卫**（`ClaudeProvider.dead_refresh`，进程内）：refresh 返回 `invalid_grant` 时把该
   refresh token 指纹记为死，`refresh_if_near_expiry` / `query_quota` 401 自愈命中即**跳过、不再发刷新**，
   止住风暴；token 一旦轮换（指纹变化）自动恢复。quota 查询则返回含 `re-login` 的错误 → CLI 压成
   `needs re-login`（`render.rs::compact_error`），不再默默挂旧缓存。**只在 `invalid_grant` 判死，
   网络/超时不判**。
2. **持续回灌（capture-on-arrival）**：daemon 每轮先调 `ClaudeProvider::reconcile_active_from_live()`
   （= `capture_live_into_store`，**只读 live、只写 store、不刷新、不写 live**，对 active 也安全），
   把当前 active 账号的 live token 持续抓回 store。缩小「绕过 swap 离开」的缺口——该账号日后变 parked
   时手里仍是较新 token。注意：Claude Code 异步轮换，回灌只能缩小窗口、**无法 100% 消除**死 token；
   彻底恢复仍需该账号重登一次。

> 改动 `activate` / keepalive / `query_quota` 自愈逻辑时务必维持本约束，别让 subswap 在
> 后台刷 active 账号、或把陈旧 token 写回 live。新增的死 token 守卫与持续回灌都遵守此约束
> （回灌只 live→store，守卫只是少刷）。

### auth.json schema 不稳定（透传策略）

Codex 经历过 schema_version v2→v3→v4 迁移。subswap 故意**不绑定具体 schema**：

- 整段 `auth.json` 当 **opaque blob** 存 CredentialStore
- 只解析少量元数据用于展示与去重：
  `account_key / email / alias / chatgpt_account_id / chatgpt_user_id / account_name / plan`
- `access_token` 仅在 quota 路径才解析，用 `extract_access_token()` **宽松递归**查找任意 JSON 位置

2026-05 观察到 Codex CLI 可生成 API-key 型 `auth.json`：

```json
{
  "OPENAI_API_KEY": "...",
  "last_refresh": "...",
  "tokens": {
    "account_id": "..."
  }
}
```

这类文件没有扁平的 `account_key/email`，但 `tokens.id_token` 的 JWT payload 通常含 `email`，
应优先用它作为 subswap 账号 id / 展示 label；`tokens.account_id` 用作 `ChatGPT-Account-Id`。
如果连 `tokens.id_token` 和 `tokens.account_id` 都缺失，subswap 只能使用 API key 的本地指纹作为
去重 id；指纹不得替代真实 secret，完整 `auth.json` 仍只存 CredentialStore。

### 切换 (activate) 触达的文件

1. 整段重写 `~/.codex/auth.json`（原子，0o600）
2. `fs2::FileExt::lock_exclusive` 在 `<codex_home>/.subswap.lock` 上加文件锁

### 与其他本地账号工具共存

- 其他工具可能维护 `~/.codex/accounts/registry.json` + `accounts/<key>/auth.json`
- subswap **不读不写**这些文件；subswap 自己的元数据在 `<config_dir>/registry.toml`
- 两个工具可共存，但不要混着用同一个账号管理

---

## Kimi / Moonshot

Kimi 是跑在共享引擎（`crates/providers/common`，见下节）上的第二个文件型 provider，实现在
`crates/providers/kimi/`。凭证整段当 opaque JSON blob 处理，与 Codex 的 `auth.json` 同一套哲学。

### 本地凭证路径

| 项 | 值 |
|---|---|
| 工作目录 | `KIMI_CODE_HOME` 环境变量 > `~/.kimi-code` > 相对路径 `.kimi-code`（`paths.rs::kimi_home`） |
| 当前激活凭证文件 | `<home>/credentials/kimi-code.json`（`paths.rs::active_cred_path`） |

### 令牌与元数据（JWT-based，无 email）

Kimi 没有邮箱概念；账号主键、展示 label 全部来自 access_token 这个 JWT 的 payload
（`kimi_files.rs::parse_metadata` / `decode_jwt_payload`）：

- `access_token` 为 JWT，payload 含 `user_id` / `client_id` / `scope`，约 **15 分钟过期**。
- `refresh_token` 有效期约 **30 天，且单次轮换**（用一次即失效）——与 Codex 同款风险，
  重复使用会被服务端拒绝。
- `primary_id` / `label` 都取 `user_id`；`scope` 落进 `registry.toml` 的 `extra["scope"]` 仅作展示。
- 没有跨主键去重键（`dedup_key` 恒为 `None`）：Kimi 的 `user_id` 本身就稳定，不像 Codex 的
  `account_key` 会轮换、需要额外去重键兜底。

### 刷新端点

| 用途 | 方法 | URL |
|---|---|---|
| Token 刷新 | POST | `{KIMI_CODE_OAUTH_HOST:-https://auth.kimi.com}/api/oauth/token` |

- 请求体 `form-urlencoded`：`client_id`（从旧 access_token JWT 的 `client_id` claim 取）、
  `grant_type=refresh_token`、`refresh_token`。
- `client_id` 或 `refresh_token` 缺失（如纯 API-only blob）→ `RefreshOutcome::Unsupported`，不发请求。
- 响应 `401` / `403`，或 body 里 `error == "invalid_grant"` → `RefreshOutcome::DeadToken`
  （与 Codex/Claude 共享同一套死 token 语义，交给引擎的 parked-only 刷新与死 token 处理）。
- 成功后把新 `access_token` / `refresh_token` / `scope` / `token_type` / `expires_in` 合并回原 blob
  结构（保留未知字段），并按 `expires_in` 换算出 `expires_at`（epoch 秒）一并写入。
- 实现：`crates/providers/kimi/src/oauth.rs::refresh_blob`。

### Usage 端点与窗口映射

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `{KIMI_CODE_BASE_URL:-https://api.kimi.com/coding/v1}/usages` |

- 请求头：`Authorization: Bearer <access_token>`。
- 响应数值字段是**字符串**（如 `"limit":"100"`），解析时须同时兼容数字与字符串
  （`kimi_usage.rs::to_u64`）；`used` 缺失时用 `limit - remaining` 推导。
- `reset_at` 取 `resetTime`（ISO8601 RFC3339）。
- 窗口映射：
  - 顶层 `usage` 对象 → 固定映射为 **7 天窗口**（`QuotaWindow::SevenDay`）。
  - `limits[]` 数组每项按 `window.duration` + `window.timeUnit` 换算成分钟
    （`MINUTE`/`HOUR`/`DAY` 单位换算），`duration:300, timeUnit:TIME_UNIT_MINUTE`（300 分钟）
    → **5 小时窗口**（`QuotaWindow::FiveHour`）；`10_080` 分钟 → 7 天；其余 → `Custom`。
  - 实现：`kimi_usage.rs::parse_usages` / `window_from_minutes` / `minutes_of`。
- 实现：`crates/providers/kimi/src/kimi_usage.rs::fetch_quota_with_active_recovery`（底层解析仍在
  `fetch_quota_at` / `parse_usages`）。

### active 401 的官方锁协调（兼容两代 Kimi CLI）

active 账号首次查询 401 后，subswap 只有在**能确定当前 Kimi 版本的官方跨进程锁协议**时，才会在同一把锁内
重读 live 凭证、必要时刷新一次、原子落盘，再重试 usage 一次：

| Kimi CLI 世代 | 官方协调机制 | subswap 行为 |
|---|---|---|
| 新 TypeScript 0.x（`--version` 输出裸 semver） | Unix 的 `oauth/kimi-code.lock/` proper-lockfile 目录锁 | 兼容目录锁、1 秒续租；连续确认超过 10 秒未续租才原子改名清理 stale 锁 |
| 新 TypeScript 0.x（Windows） | 上游当前没有等价跨进程锁 | active 保持 401，**绝不刷新** |
| 旧 Python `>= 1.31.0` | `credentials/kimi-code.lock` 文件锁 | 使用同一路径与 flock 协议 |
| 旧 Python `< 1.31.0`、版本未知或无法执行 | 无可证明兼容的锁 | 安全降级，active 保持 401 |

TypeScript 客户端设置 `KIMI_DISABLE_OAUTH_LOCK=1` 时，subswap 也必须禁用 active 刷新；不能在上游明确关锁后
单方面加锁。持锁后先重读 `kimi-code.json`：若官方客户端或另一 subswap 进程已经轮换，只复用最新 access token；
账号不匹配则退出；只有 access token 仍是本次失败那枚时才刷新。

刷新成功必须在释放锁前以临时文件 + rename 原子替换 live 凭证；`invalid_grant` / 401 / 403 只保存 refresh token
的 SHA-256 指纹，不保存 secret。后续命中同一指纹不再发请求；token 变化后自动恢复。此持久守卫同时避免 CLI
和 daemon 不同进程反复撞同一枚死 token。网络错误或超时不能判死。

这套方案比「后台启动一次 kimi，等它自己刷新再关掉」更安全：TUI 是否会刷新、何时落盘都不构成稳定契约，
而且强行启停会打断用户会话；官方锁才是能够证明不会并发消耗一次性 refresh token 的边界。

### 测试环境变量重定向

`KIMI_CODE_OAUTH_HOST` / `KIMI_CODE_BASE_URL` 分别覆盖刷新与 usage 端点的 base URL，供集成测试
指向本地 mock server，避免测试打真实 Moonshot 端点。同 `KIMI_CODE_HOME` 一样是纯环境变量覆盖，
无需额外配置文件支持。

### 登录方式（无 subswap 驱动的 OAuth 流程）

Kimi 没有官方 CLI 子命令可供 subswap 像 `codex login` / `claude auth login` 那样驱动登录。
约定：用户自己先跑 `kimi` 这个原生 TUI 完成登录，`subswap login kimi` 只是把当前
`~/.kimi-code/credentials/kimi-code.json` 导入 subswap（`FileBlobProvider::import_active`），
不发起任何 OAuth 网络请求。`--email` / `--sso` / `--device-auth` 对 kimi 登录一律不支持。

---

## OpenCode Go

OpenCode Go 是跑在共享引擎上的第三个文件型 provider，实现在 `crates/providers/opencode/`。
它**不是**整份 `auth.json` 的切换：官方 `~/.local/share/opencode/auth.json` 是多供应商共存的 map
（`openai` / `anthropic` / `opencode-go` 等可以同时存在）。subswap 只抽出、只覆盖 `opencode-go`
这一项，其它条目必须原样保留。引擎为此提供 `extract_blob` / `compose_live` 两个 hook，默认仍是
整文件覆盖（Codex/Kimi 不用改）。

Go 订阅本身是一把 API key（`{"type":"api","key":"sk-..."}`），没有 refresh token，不刷新。

### 本地凭证路径

| 项 | 值 |
|---|---|
| 工作目录 | `SUBSWAP_OPENCODE_HOME` > `XDG_DATA_HOME/opencode` > `~/.local/share/opencode`（Windows 为 `%LOCALAPPDATA%/opencode`）。官方客户端用 xdg-basedir，**macOS 也是 `~/.local/share`，不是 Application Support** |
| 当前激活凭证文件 | `<home>/auth.json` |
| live 文件中的键 | `opencode-go` |

### 主键与展示名

- `primary_id` / `dedup_key` = `go-` + API key SHA-256 的前 16 个 hex。同一把 key 重复导入落到同一账号。
- `label` = `sk-…` + key 末 4 位。
- store 里只存 `opencode-go` 那一项 JSON，不存整份 `auth.json`。

### Usage 端点与窗口映射

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `{SUBSWAP_OPENCODE_GO_BASE:-https://opencode.ai/zen/go/v1}/usage` |

- 请求头：`Authorization: Bearer <api_key>`，`User-Agent: subswap/<version>`。
- 实测响应：

```json
{
  "usage": {
    "rolling": { "status": "ok", "percent": 4, "resetsAt": "2026-08-13T16:27:38.287Z" },
    "weekly":  { "status": "ok", "percent": 3, "resetsAt": "2026-08-17T00:00:00.287Z" },
    "monthly": { "status": "ok", "percent": 1, "resetsAt": "2026-09-13T06:06:01.287Z" }
  }
}
```

- `percent` 是已用百分比（0~100），写入 `Quota.used`，`limit` 固定 100。
- 窗口：`rolling` → 5 小时，`weekly` → 7 天，`monthly` → 月。
- `status: "rate-limited"` 视为该窗口 100% 已用。
- `401` / `403` 表示 key 无效或没有有效 Go 订阅，按「需要重新导入 key」处理；**不得**把 `429` 当成 key 作废。
- 自动换号：`rolling` 映射为小时级窗口，用量过默认阈值会切走；weekly/monthly 只在明确耗尽时触发/阻断候选。daemon 与默认入口都已注册该 provider，策略本身按 provider 独立决策，不需要单独的 OpenCode 规则。
- 测试可用 `SUBSWAP_OPENCODE_GO_BASE` 指向 mock。

### 隔离运行

官方客户端没有 `OPENCODE_HOME`。隔离同时做两件事：

1. 设 `XDG_DATA_HOME` 到私有目录，并把合成后的 `auth.json` 写到 `<env>/opencode/auth.json`。
2. 设 `OPENCODE_AUTH_CONTENT` 为同一份 JSON（`{"opencode-go":{...}}`）。官方客户端在此变量存在时完全忽略磁盘上的 `auth.json`。

### 登录方式

没有可供 subswap 驱动的官方登录子命令。两种导入：

- `subswap login opencode`：从当前 live `auth.json` 的 `opencode-go` 项导入（用户先在 TUI `/connect` 粘贴过 key）。
- `subswap login opencode -- sk-...`：直接导入粘贴的 API key，并合并写回 live `auth.json`。

`--email` / `--sso` / `--device-auth` 不支持。

### 开源圈「号池」≠ subswap 切号（2026-08-16 调研）

社区里叫 OpenCode 号池的工具很多，但和 subswap 做的不是同一件事。改 OpenCode 自动换号前必须先分清，否则会把「请求途中换 key」误做成「改本地登录文件」。

**A. 登录文件切换器**（和 subswap 同类：一次只让官方客户端认一个 Go 号）

| 项目 | 做法 | 不照搬 |
|---|---|---|
| `srmdn/opcode-switch`（已迁 `opcode-kit`） | 每个号一份快照，切换时整份覆盖本地登录文件 | 整文件覆盖会抹掉同文件里其它供应商；subswap 只改 `opencode-go` 那一项 |
| `@ceritahmt/opencode-as` | 按供应商抽出登录项做 profile；看到「用量上限」文案后可选自动切到同供应商下一个 profile | 靠错误文案，不是额度接口；切完通常要重开客户端 |
| `farion1231/cc-switch` | 桌面端把 `auth.json` 当独立凭证仓，自定义供应商定义与 key 拆开写 | 管的是「多个供应商配置」，不是 Go 订阅号池 |

**B. 请求途中号池**（社区说的「号池」多半是这类：当前会话不换登录文件，限流时当场换下一把 key 再发）

| 项目 | 做法 | 要点 |
|---|---|---|
| `dhaalves/opencode-swap`（`oswap`） | 本机代理挡在官方 Go 接口前，轮询 key，限流当场换下一把再试 | README 写明：插件钩子拦不到限流响应，所以走代理。上游 `Retry-After` 有时给的是**周重置日期**，只是滚动窗口撞限也会被理解成冷却好几天，他们把冷却上限封在 1 小时 |
| `masrurimz/opencode-go-multi-auth` | OpenCode 插件：进程内粘滞一把 key（保上下文缓存），限流再换 | **不做提前查额度**，只反应限流。号池存在插件自己的配置文件，不改官方登录文件 |
| `Rishabh-Bajpai/opencode-go-multi-auth` | 插件拉起本机路由 + 看板，402/429 冷却后换 key | 同样不预测额度；要改官方客户端的接口地址指向本机路由 |
| `bytesifter/opencode-round-robin` | 插件补丁全局请求：随机抽 key；限流只冷却、**当次不重发** | 把「请求太快」和「额度用尽」分成两种冷却时长 |
| `rahadiana/opencode-multi-account` | 通用多供应商号池插件，限流后按优先级切 | 会回写登录文件里对应供应商项；插件作者自己也在问官方：事件钩子经常看不到 429 |

官方 OpenCode 正在给 **OAuth** 做同供应商多账号轮换（限流换下一份凭证），**不覆盖 Go 这种纯 API key**。Go 号池仍是第三方插件/代理的活。

**对 subswap 的边界**

- 已落地的是 A：查官方用量、过阈值改本地 `opencode-go` 项。这能在下次启动/下次轮询时换号，**挡不住当前这次请求已经撞上的限流**。
- 社区 B 才是「打着打着自动换下一把、用户无感」。要做到这一点，必须进 OpenCode 进程（插件）或挡在接口前面（本机代理）。subswap 作为外部切号器做不到当场重发。
- 部分新版本还会在 `account.json` 里再存一份 Go 凭证（`oswap import` 会同时读它）。只写 `auth.json` 时，若官方已改读 `account.json`，切号会看起来没生效。改 OpenCode 切换前先核当前客户端读的是哪份文件。
- 旧插件文档常写「Go 没有用量接口」。官方已有 `GET …/zen/go/v1/usage`；那是过时结论，不要跟着放弃提前查额度。

---

## Cursor

Cursor 不是文件型 JSON Provider：登录状态位于本地客户端存储，切换时还可能需要协调 GUI 退出与重启，
因此独立实现 `Provider`，不接 `crates/providers/common`，也不支持 `subswap run/shell/env` 隔离运行。

Cursor 有两种客户端，凭证存储布局不同，subswap 用 `CredentialSource` 枚举统一抽象，两者共用同一套额度查询
与 refresh token 轮换逻辑（代码见 `crates/providers/cursor/src/lib.rs::CredentialSource`）：

- **桌面版（Electron IDE）**：凭证在 SQLite 数据库 `state.vscdb` 的 `ItemTable`。
- **命令行 agent（`cursor-agent`）**：邮箱等元数据在 `~/.cursor/cli-config.json` 的 `authInfo`。
  token 存放随平台与官方开关变化，**不是永远一份 JSON 文件**：
  - macOS 默认写系统钥匙串（service `cursor-access-token` / `cursor-refresh-token`，account `cursor-user`），
    不落盘；
  - 官方文件后端时：macOS 为 `~/.cursor/auth.json`，Linux 为 `~/.config/cursor/auth.json`（或 `$XDG_CONFIG_HOME/cursor/auth.json`）。
  无 GUI 生命周期，适用于服务器等无桌面环境。

来源自动探测顺序：显式指定桌面库时始终用桌面版；否则桌面库能读出有效登录时用桌面版。桌面库只是未登录的遗留文件
而命令行已登录（macOS 钥匙串有 access token，或官方 `auth.json` 有 access token）时，回退命令行来源，避免默认入口
静默遗漏 CLI 账号；两者都不存在时回退桌面路径，读取时给出「请先登录」提示。

macOS 命令行钥匙串读写**只能 fork `/usr/bin/security`**，禁止 `keyring` crate，否则会把官方条目 ACL 改成「仅 subswap」，
`cursor-agent` 下次读取反复弹授权框。同类事故见
[troubleshooting/2026-06-11-claude-code-keychain-acl-poisoning.md](troubleshooting/2026-06-11-claude-code-keychain-acl-poisoning.md)。
**已有条目只更新密码、禁止 delete 后再 add**：删建会把解密权限收成「仅 security」，桌面版邮箱显示对了、请求却报未登录。
新建条目时才把 `/usr/bin/security` 与 Cursor.app 写入信任名单。曾漏读钥匙串、只认 Linux 风格登录文件的故障见
[troubleshooting/2026-08-14-cursor-quota-missing-cli-keychain.md](troubleshooting/2026-08-14-cursor-quota-missing-cli-keychain.md)。

**live 归属以令牌 JWT 为准，禁止把当前令牌拼到过期身份上。** 命令行身份文件（`authInfo`）与令牌不在同一处，
只写令牌、留下上一号邮箱时，回灌和额度查询会把同一份令牌算到每个账号上，停用号再刷新还会把真正主人的一次性
refresh token 刷废。匹配主人只认 access token 的 JWT `sub`；`authInfo.authId` 与 JWT 不一致时忽略这份身份，
回灌不得用过期邮箱覆盖主人；仓库里令牌 JWT 对不上该账号时显示 `needs re-login`，禁止查询或刷新。
现象与修复见
[troubleshooting/2026-08-14-cursor-quota-cloned-across-accounts.md](troubleshooting/2026-08-14-cursor-quota-cloned-across-accounts.md)。

### 本地状态与跨平台路径

桌面版 `state.vscdb`：

| 平台 | 默认路径 |
|---|---|
| macOS | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` |
| Linux | `~/.config/Cursor/User/globalStorage/state.vscdb` |
| Windows | `%APPDATA%\Cursor\User\globalStorage\state.vscdb` |

命令行 agent 元数据：`~/.cursor/cli-config.json`。token：macOS 默认钥匙串；文件后端时 macOS 为
`~/.cursor/auth.json`，Linux 为 `~/.config/cursor/auth.json`。subswap 按官方路径探测，macOS 优先读钥匙串。

测试用 `SUBSWAP_CURSOR_STATE_DB_PATH`（桌面版）或 `SUBSWAP_CURSOR_AGENT_AUTH_PATH` /
`SUBSWAP_CURSOR_AGENT_CONFIG_PATH`（agent 文件后端）重定向到绝对临时路径；macOS 命令行钥匙串用
`SUBSWAP_CURSOR_KEYCHAIN_PATH` 指到一次性 keychain。相对路径会直接报错，完整隔离契约见
[OPERATIONS_GUIDE.md](OPERATIONS_GUIDE.md) 的「三平台测试隔离」。桌面版只读写 `ItemTable` 中与身份有关的键：
`cursorAuth/accessToken`、`cursorAuth/refreshToken`、`cursorAuth/cachedEmail`、`cursorAuth/authId`，并同步
兼容键 `cursor.accessToken` / `cursor.email`；其余 Cursor 设置、扩展和工作区状态不动。agent 写回时
令牌与 `cli-config.json` 的 `authInfo` 成套覆盖：文件后端写 `accessToken` / `refreshToken`，钥匙串后端写
对应条目，并同步邮箱 / authId；保留文件里的其他字段。两种来源在 CredentialStore 中都保存同构的私有
JSON blob，registry 只存邮箱、稳定身份与展示元数据。

### 登录、导入与切换事务

`subswap login cursor` 不复制 Cursor OAuth，也不驱动网页登录：用户先在 Cursor 客户端登录（桌面端登录，
或 `cursor-agent login`），命令只读取本地凭证、导入/覆盖账号并标记 active。默认入口会同步当前 live 账号（与
Claude / Codex / Kimi 相同）；`rm` 过的号只要客户端仍登录着，下次默认入口就会照常收回来——不再有「记住删除」
的墓碑机制拦截（2026-08-14 引入、2026-08-15 因导致 Cursor 账号无声消失而移除，见
[troubleshooting/2026-08-15-cursor-section-silently-missing.md](troubleshooting/2026-08-15-cursor-section-silently-missing.md)）。

**agent 来源的切换**是纯本地凭证写回：capture-on-leave 回灌当前 agent 登录 → 快照旧令牌、`cli-config.json` 与 registry →
把目标账号的令牌写回当前后端（文件或 macOS 钥匙串）并同步 `authInfo` → 标记 registry active；失败则令牌、配置与
registry 一起回滚。无进程协调，写回后 `cursor-agent` 下次读取即生效。

**桌面版来源**的 Cursor 进程存活时不能直接改 SQLite：Electron 退出阶段可能把内存中的旧 token 写回数据库，
覆盖 subswap 刚写的账号。切换固定遵守以下顺序：

1. 检测 Cursor 是否运行；运行中则请求正常退出，并等待进程完全结束，超时则不做切换。
2. 读取 live 凭证并 capture-on-leave；若 live 缺 refresh token，绝不能覆盖仓库中有 refresh 的副本。
3. 快照六个身份键，在一个 SQLite transaction 内写入目标 blob并提交，再标记 registry active。
4. 切换前 Cursor 在运行时，成功后重新打开并确认进程启动。
5. 数据库写入、active 标记或重启任一步失败，都恢复数据库与 registry；若原来在运行，重新打开旧会话。

macOS 用系统退出事件，Linux 用 TERM，Windows 用不强杀的 `taskkill /IM Cursor.exe`；三端都等待退出完成。
这条业务路径允许短暂关闭并重开 Cursor，是为了保证账号切换不会被客户端退出写回反向覆盖。

### 额度与刷新边界

| 用途 | 方法 | URL |
|---|---|---|
| 用量查询 | GET | `https://cursor.com/api/usage-summary` |
| parked token 刷新 | POST | `https://api2.cursor.sh/oauth/token` |

usage 查询从 access token 的 WorkOS subject 生成官方 session cookie，不使用 Bearer header。解析
`individualUsage.plan`（兼容 snake_case / `planUsage`）里的：

- `autoPercentUsed` → 展示标签 `1st`（Cursor 官方模型窗口），写入 `Quota.used` 后 CLI 按 `{100-N}% left` 展示；
- `apiPercentUsed` → `API`，写入 `Quota.used` 后 CLI 按 `{100-N}% left` 展示；
- `billingCycleEnd` → 两个窗口共同的 reset 时间。

这两个百分比写入统一 `Quota.used`，没有额外翻转；CLI 展示层与其他 Provider 一样转成余量。

active 查询 401 时**绝不刷新**：只重读 live 数据库。若 Cursor 已经自行轮换 access token，则 capture 回仓库并
用新 token 重试一次；否则返回认证错误。parked 账号没有原生客户端维护，允许在 subswap 自己的跨进程文件锁
内刷新：锁内重读仓库，若另一进程已轮换就直接复用；否则只刷新一次并持久化完整 token pair。401/403 或
`shouldLogout` 会把 refresh token 的 SHA-256 指纹写成 dead guard，同一死 token 后续不再请求，变更后恢复。

---

## 文件型 OAuth 切换共享引擎（`crates/providers/common`）

Codex 和 Kimi 都是"凭证是本地一个 JSON 文件、切换 = 原子覆盖这个文件"这类 provider，
两者的机制部分（切换/回滚/回灌/隔离）完全一致，只有解析凭证、刷新、查额度的细节不同。
这部分公共机制被抽成 `subswap-provider-common` crate，避免未来每加一个同构 provider
就重新实现一遍 flock/snapshot/capture-on-leave 这套易错逻辑。

**Claude 不在这个引擎上**：Claude 走 macOS Keychain（而非本地文件）+ 有自定义 API 账号这种
无凭证文件的特殊账号类型，形状和"本地 JSON blob"完全不同，继续保留 `crates/providers/claude`
的独立实现（见上方 §Claude 的「切换 (activate) 触达的文件」）。

### 引擎（`FileBlobProvider<A: FileBlobRuntime>`，`engine.rs`）负责什么

引擎实现完整的 `Provider` trait，adapter 只需实现 `FileBlobRuntime`（见下表）：

| 机制 | 说明 |
|---|---|
| `activate` 原子切换 | flock → snapshot 旧文件 → 原子写新 blob（tmp+rename+0600）→ 任一步失败回滚 |
| capture-on-leave | 覆盖 live 文件前，先把当前 live 凭证回灌进它所属账号的 store 副本；带「缺 refresh 快照不覆盖有 refresh 副本」的守卫，防止静默写死账号 |
| capture-on-arrival | `reconcile_active_from_live`：只读 live、只写 store、不刷新、不写 live，供 daemon 每轮补「绕过 swap 直接在原生 CLI 里切换」的缺口 |
| parked-only 刷新 | `query_quota` 只对非 active（parked）账号调 `runtime.refresh()`；active 账号只读不刷，避免与原生客户端抢刷同一份 refresh token |
| 取 blob 的 fallback 链 | `raw_blob_for_account`：active 优先读 live（顺手修复 store 副本）→ store → `recover_legacy`；store 读失败时不立即冒泡，先试 legacy，只有都失败才把原始 store 错误抛出 |
| 隔离导出/吸收 | `export_blob` / `absorb_blob`，供 `IsolatedProvider`（`isolated.rs`）在 `subswap run/shell/env` 里做物化与会话结束吸收；任何 `FileBlobRuntime` 都通过 blanket impl 自动获得 `IsolatedProvider`，无需 provider 自己写隔离逻辑 |
| 导入 | `import_active` / `sync_active_metadata`（只对齐 active 标记不重写 blob，供默认入口避免弹钥匙串）/ `import_raw` / `import_raw_with_explicit_metadata`（legacy registry 迁移场景，保留调用方提供的 metadata 不重新派生） |

### Adapter（`FileBlobRuntime` trait，`runtime.rs`）必须提供的差异点

| 方法 | 用途 | Kimi | Codex（迁移前存量数据，需覆盖默认值） |
|---|---|---|---|
| `id()` / `display_name()` | provider 标识 / 展示名 | `"kimi"` / "Kimi / Moonshot" | `"codex"` / "Codex / ChatGPT" |
| `home()` | 工作目录解析 | `KIMI_CODE_HOME` 等 | `CODEX_HOME` 等 |
| `live_cred_path()` | live 凭证文件路径 | `<home>/credentials/kimi-code.json` | `<home>/auth.json` |
| `parse_metadata()` | 从 blob 抽 `BlobMetadata`（primary_id/label/dedup_key/extra） | 解 access_token JWT 取 `user_id` | 解析 `auth.json` 拿 `account_key`/`email`/`chatgpt_account_id` 等 |
| `refresh()` | 刷新一次，返回 `RefreshOutcome` | 真刷（`oauth.rs`） | `Unsupported`（Codex CLI 自己刷，subswap 不做带外刷新） |
| `fetch_quota()` | 查额度 | `GET /usages` | `openai_usage` + legacy 缓存回退 |
| `isolation()` | 隔离环境变量名 + 原生 CLI 名 | `KIMI_CODE_HOME` / `kimi` | `CODEX_HOME` / `codex` |
| `extract_blob()` / `compose_live()`（可选，默认整文件） | 多供应商共存的 live 文件只抽出/写回本 provider 那一项 | 不覆盖 | 不覆盖 |
| `access_token()`（可选，默认找 `access_token`） | 从 blob 抽额度查询 token | 不覆盖 | 不覆盖 |
| `isolation_rel_path()` / `isolation_extra_env()`（可选） | 隔离目录内相对路径、额外环境变量 | 不覆盖 | 不覆盖 |
| `store_field()`（可选，默认 `"blob"`） | 凭证仓库里存 blob 的字段名 | 不覆盖，用默认 `"blob"` | 覆盖为 `"auth_json"`——兼容迁移前已写在 keyring/FileStore 里的存量字段名 |
| `dedup_extra_key()`（可选，默认 `"dedup_key"`） | `registry.toml extra` 里去重键的字段名 | 不覆盖（Kimi 本就无 dedup_key 需求） | 覆盖为 `"chatgpt_account_id"`——迁移前 `registry.toml` 里已有这个键名，沿用旧名免去给所有存量账号重新导入 |
| `recover_legacy()`（可选，默认 `None`） | store/live 都拿不到时的私有兜底恢复 | 未用 | 从 `~/.codex/accounts/registry.json` + `accounts/<key>/auth.json` 恢复（`legacy.rs`） |
| `materialize_extra()`（可选，默认空） | 隔离物化时的额外动作 | 未用 | 复制真实 `~/.codex/config.toml` 进隔离目录（`legacy.rs::copy_codex_config_best_effort`） |

**新增一个全新（无存量数据）的文件型 provider时**：只需实现前 8 个必填方法 + `isolation()`，
`store_field()` / `dedup_extra_key()` 两个 hook 保持默认值即可，不必理会 Codex 那两行覆盖——
它们只是为了不强迫 Codex 存量用户重新导入账号而存在的历史兼容口子。

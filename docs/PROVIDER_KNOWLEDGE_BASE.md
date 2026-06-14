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
| CLI 展示 | **余量**：`{limit - used}% left`，不打印 ok/warn/full 文字，严重程度靠余量数字 + 颜色(warn 黄 / full 红) | `render.rs::format_quota_compact` |

记忆点：**两个 Provider 的百分比语义是一致的（都是已用），不存在「一个用量一个余量」**。
之所以容易误读，是因为已用值高（如 59%）直觉上像「剩很多」。展示层显示 `41% left` 就是为了消除这个歧义。
改展示格式时**不要去翻转 `Quota.used` 的语义**，只在 `format_quota_compact` 里做 `limit - used`。

**既定 UX 约定（勿改回）**：CLI 一律显示**余量** `{N}% left`，不显示已用%、不打印 ok/warn/full 文字。
理由：用户关心的是「还能用多少」，余量比用量直观；严重程度由余量数字本身 + 颜色（warn 黄 / full 红）传达，
文字状态块冗余。后续若想加用量视图，作为可选项叠加，别把默认换回用量。

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
| `401` | usage 401 | active 账号 live token 过期、Claude Code 未刷 | 开一次 Claude Code 让它刷；subswap 不刷 active |

**缓存节流（治 429，`crates/cli/src/cmd/default.rs` + `crates/daemon/src/unix.rs::build_snapshots`）**：
subswap 每次 CLI 运行 + daemon 每轮都把所有账号一起查，极易并发打爆 usage 端点 → 全员 429。
对策：daemon 与 CLI **共用** `quota_cache.json`，查询前先看缓存——`QuotaCache::fresh()` 判定缓存
比 `settings.quota.min_refresh_interval_ms`(默认 90s，> daemon 60s 轮询) 新就**直接复用、不打端点**。
谁先查到谁刷新 `cached_at`，另一方据此跳过，把每账号请求频率稳定压到 ~90s 一次。

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
    "scopes": ["user:inference"]
  }
}
```

其他字段通过 `#[serde(flatten)]` 透传保留，避免上游加字段时丢失。

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
Claude 的主模型 / Opus / Sonnet / Haiku / subagent 角色映射到 DeepSeek 模型。

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
- 非活跃账号的 `access_token` 只存在 keyring 里，没人帮它刷，**subswap daemon (M4) 负责后台自动保活**：
  周期扫描 `expires_at`，临近过期且有 `refresh_token` 时调 Anthropic OAuth 端点 + 写回 keyring。
- 不暴露 `subswap refresh` 子命令；保活是应用后台职责，不进入日常用户工作流。

### Codex Token 刷新（subswap 不做）

不实现 Codex refresh，理由：

1. **Codex CLI 自己刷新**。`auth.json.tokens.refresh_token` 在 Codex CLI 启动时由它自己拿去
   调 OpenAI OAuth 端点换新的 `access_token`，再写回 `~/.codex/auth.json`。
2. **抢写风险**。subswap 主动刷会与 Codex CLI 同时写同一个文件，需要更强的锁协议，得不偿失。
3. **OpenAI OAuth client_id 不是公开常量**。和 Anthropic 的 `9d1c250a-...` 不同，
   盲目硬编码风险大。
4. **避免维护非公开 OAuth 协议**。首次登录由官方客户端完成，subswap 不复制刷新流程。

**用户表现**：切到一个长期未用的 codex 账号、Codex CLI 启动时立即报 401 →
解决办法是在 Codex 客户端里重新登录，然后重新运行 `subswap` 让它自动导入当前激活账号。

Claude 那边的保活由 subswap daemon (M4) 自己做，因为非活跃 Claude 账号的凭证只存在 keyring 里、
没有 Claude CLI 帮它刷；Codex 没这个问题（所有账号最终都流经 `~/.codex/auth.json`，Codex CLI 持续维护）。

### Refresh token 轮换与 capture-on-leave（核心安全约束）

**两边的 refresh token 都是一次性轮换**：刷新一次旧 token 立即作废。subswap 与原生客户端
（Codex CLI / Claude Code）若各自独立持有同一份 refresh token 并各自刷新，必然有一方被服务端
作废，表现为 `refresh token already used` 强制重登（排查见
[troubleshooting/2026-06-08](troubleshooting/2026-06-08-codex-refresh-token-already-used.md)）。

**不变量：原生客户端是 active 账号 live token 的唯一轮换者。** subswap 对 active 账号只读不刷；
只对停泊（parked）账号刷新/恢复。落地两个机制：

1. **Capture-on-leave**：`Provider::activate` 在覆盖 live 文件前，先读当前 live 凭证、找受管
   owner 账号、回写其 store（`capture_live_into_store`，Codex/Claude 各一份）。否则切走的账号
   store 副本会停在旧 token，下次切回写回旧 token → 作废。所有 swap（手动 + daemon 自动）唯一
   经过 `activate`，一处生效覆盖两条路径；找不到 owner 直接跳过（best-effort，不阻塞 swap）。
   Claude 重复切换当前账号时只执行回灌并直接返回，禁止先读 store 再把陈旧 token 覆盖回 live。
2. **绝不轮换 active 账号 token（仅 Claude，Codex 本就不刷）**：
   - `refresh_if_near_expiry` 开头加 active 守卫（`active_account_id()` 命中即返回 `Ok(false)`），
     daemon 后台保活只对 parked 账号生效。
   - `query_quota` 401 自愈仅当凭证来自 store（parked）才刷新；来自 live（active）直接返回错误，
     交还 Claude Code 自刷。
   - macOS 的 active 凭证读取必须优先 Claude Code Keychain；`.credentials.json` 只是兼容副本，
     不能用它覆盖或查询当前账号。

Quota 查询遇到 `401` / `403` / `429` 时不重试；尤其 `429` 连续重试会延长 `quota loading`
并加重服务端限流。

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

- 整段 `auth.json` 当 **opaque blob** 存 keyring
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
去重 id；指纹不得替代真实 secret，完整 `auth.json` 仍只存 keyring。

### 切换 (activate) 触达的文件

1. 整段重写 `~/.codex/auth.json`（原子，0o600）
2. `fs2::FileExt::lock_exclusive` 在 `<codex_home>/.subswap.lock` 上加文件锁

### 与其他本地账号工具共存

- 其他工具可能维护 `~/.codex/accounts/registry.json` + `accounts/<key>/auth.json`
- subswap **不读不写**这些文件；subswap 自己的元数据在 `<config_dir>/registry.toml`
- 两个工具可共存，但不要混着用同一个账号管理

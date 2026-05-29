# Provider 知识库

记录各 Provider 实际使用的上游接口、本地文件、认证字段等「代码不能表达」的事实。
代码本身能表达的内容（函数签名、struct 字段、import 关系）不在此重复。

> 新加 Provider 时请按本文档结构补一节。

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

**单位漂移防御**：utilization 当前是 0~100；若上游某天改成 0~1 比例，
`make_quota` 已加 sanity 归一化（值 ≤ 1.5 视作比例 × 100）。

### Usage 接口异常状态码的真实含义

`/api/oauth/usage` 在 token 出问题时不会老老实实回 401，会**把鉴权失败伪装成 429**。
实测：access_token 过期且本地没有可用 refresh_token（或刷新失败）时，接口返回
HTTP 429 + body 含 rate-limit 字样的话术，而不是 401。

排查含义：

- subswap 表面看是 quota 拉不下来 / AutoSwap 把账号判成 `Exhausted`，根因可能是 token 失效。
- 真实限流不会持续超过一个 5h 窗口；如果某账号连续多次只在 usage 接口报 429、其他 Claude
  Code 调用也立刻 401 → 按 token 过期处理。
- 处理路径：对该账号重新 `subswap login claude`（或直接 `claude auth login --claudeai`
  覆盖凭证），让 subswap 重新 import 一遍刷新过的 token。
- subswap 本身不主动把 429 翻译成 401：`oauth.rs::fetch_usage` 故意保留原始状态码进
  `Error::QuotaFetch`，避免对一类异常做错误归因；CLI 渲染时统一压成 `429 rate limited`
  这种短文案，所以排查时要靠 `--log debug` 看原始 message 或参考本节。

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

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
   └─ N 个 query_quota 并发；每个账号返回后刷新对应行
      （交互终端渐进刷新；非交互/管道只输出最终状态）

④ auto_decide（纯函数，无 IO）
   └─ AutoSwapPolicy：usage_ratio >= defaults::AUTO_SWAP_THRESHOLD → Swap
                     active quota 查询失败 → Degraded（提示手动 swap）
                     否则 → NoOp

⑤ 如需切换：Provider.activate(to) → write audit

⑥ render 最终状态
```

`find_unique(id)` 支持全局 id 反查（唯一时省略 provider；歧义时用 `<provider>/<id>`）。

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

### 3.3 `subswap swap <id>`

```
find_unique(id) → Provider.activate(id)
   ├─ best-effort refresh（若 token 近过期）
   ├─ spawn_blocking { flock → snapshot → 写文件 → 写 registry }
   └─ 写 audit
```

**重要**：此路径不依赖 `query_quota`，网络完全不通时仍可用。

### 3.4 `subswapd` daemon（M4）

```
每 defaults::DAEMON_POLL_INTERVAL_MS（默认 60s）：
   ├─ build_snapshots → auto_decide → activate（如需）
   ├─ 对非活跃 Claude 账号：若 expires_at < now + REFRESH_SLACK_MS → refresh_account
   └─ 写 audit
```

降级路径见 [AUTO_SWAP_DESIGN.md](AUTO_SWAP_DESIGN.md#降级到手动)。

## 4. 凭证与文件布局

### 4.1 keyring（敏感）

```
service: subswap
key:     {provider}:{account}:{field}
field 例： access_token / refresh_token / oauth_metadata
```

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
- Claude：`~/.claude/` + `~/.claude-swap-backup/`

subswap 切换时**写**这些上游目录，但**只读不存** token 元数据（token 已在 keyring）。

## 5. 扩展新 Provider 的步骤

1. 新建 `crates/providers/<id>/` crate，依赖 `subswap-core`。
2. 实现 `Provider` trait（`list_accounts / activate / query_quota / client_targets`）。
3. 在 `crates/cli/src/main.rs::AppContext::build()` 注册一行。
4. 在 `crates/cli/Cargo.toml` 加依赖；在 `sync_local_active()` 加 import_active 调用。
5. 在 `docs/PROVIDER_KNOWLEDGE_BASE.md` 补该 Provider 的接口/坑笔记。

不要在 `core` 里写任何 Provider 特定逻辑。

## 5.5 数值调优常量的管理

**所有跨模块调优参数集中在 `crates/core/src/defaults.rs`**，不允许各模块各自硬编码。

| 常量 | 默认值 | 说明 |
|---|---|---|
| `AUTO_SWAP_THRESHOLD` | `0.99` | AutoSwap 触发阈值（0.0~1.0） |
| `QUOTA_WARN_PCT` | `90.0` | Quota 视觉 Warn 阈值（百分比） |
| `QUOTA_EXHAUSTED_PCT` | `100.0` | Quota Exhausted 阈值（百分比） |
| `REFRESH_SLACK_MS` | `300_000` ms | token 预刷新提前量（5 min） |
| `AUTO_SWAP_COOLDOWN_MS` | `300_000` ms | 切换后冷却期（daemon，M4） |
| `DAEMON_POLL_INTERVAL_MS` | `60_000` ms | daemon 轮询周期（M4） |

改数值只改 `defaults.rs` 一处，AGENTS.md 不变量 #5 同步标注当前值。

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

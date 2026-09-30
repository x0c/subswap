# 自动切换设计

## 0. 核心不变量

**手动 `subswap swap` 命令永远独立于额度查询。** 即使 quota 接口、网络、凭证密钥任一不可用，
手动切换都必须能跑通。自动切换是「条件具备时锦上添花」，**不能成为切换的唯一通路**。

## Automatic swap safety requirement (2026-09-30)

Across every provider, the default entry and daemon must preserve the current account unless a completed quota query confirms its switching condition and another account is confirmed usable now. A faster candidate response, loading, empty/unknown quotas, quota-query errors (including quota endpoint 429), or stale cached exhaustion never establish that the current account must be replaced. Business-request rate limits are a separate signal; quota-query failures must not masquerade as that signal.

When no confirmed usable target exists, preserve the current selection and degrade to manual action. Do not automatically move to a failed/unknown target or another depleted account merely because it resets sooner. Continue progressive quota collection so a confirmed depleted active account can switch as soon as a usable candidate becomes ready. Healthy accounts remain selected regardless of another account's larger balance or earlier reset. Preserve Cursor's parallel-pool semantics and all manual-only/manual-hold rules.

### All-exhausted fallback (2026-09-30, user decision)

The paragraph above is partially superseded: when **every** account is confirmed unusable, staying put can leave the user on the slowest-recovering account. The default entry and daemon now fall back to the confirmed-depleted account that recovers soonest, in every provider. The ban on failed/unknown/stale targets stands — the fallback pool only admits `Ready` accounts whose gating windows are all confirmed (limit > 0, status known) and already unusable, ranked by earliest known gating reset. Warn-only active accounts (still serving) never fall back into a depleted account; manual-only, manual hold, cooldown, and the flap/oscillation brake keep working unchanged. Known cost: on Codex the swap still rewrites live `auth.json`, so running sessions need a restart even though the target is depleted right now.

## 1. 触发策略（阈值 + 限流双触发）

### 1.1 阈值触发

- 默认阈值：由 `crates/core/src/defaults.rs::AUTO_SWAP_THRESHOLD` 定义，运行时可由 `config.toml` 覆盖。
- OpenCode 仅官方 Console 账号（`opencode`）参与自动换号；`opencode-api-key` 是独立的仅手动账号池，Key 不能成为自动候选，处于当前 Key 时也不会自动切走。
- 适用窗口：只看小时级（当前可可靠识别 `FiveHour`）。Claude 7d、Codex 月度、OpenCode weekly/monthly 等长窗口即使接近阈值也不触发。OpenCode `rolling`（约 5 小时）映射为 `FiveHour`，走阈值触发。
- 硬阻断：对 **Claude / Codex 等叠加上限**，任一参与自动切换的窗口 `Exhausted` 即触发/阻断。
  **Cursor 例外**：`1st`、**Credits**、**API** 是并行可用池——任一池仍有余量即可承接；
  全部耗尽才切（`cursor_parallel_pools` 要求 `provider == "cursor"`）。其它 provider
  （如 Command Code）即使发出 `Credits` 窗口，仍按叠加语义处理。
  仅当所有池都耗尽才触发/阻断。因此「全员 1st 见底、某号 API 仍有 10%」必须切到该号，
  **禁止**按重置时间优先挑全空号。反过来：`1st` 仍有余量时，也不要因 API 耗尽就切走
  （见 2026-08-21）。实现见 `auto_policy` 的 `cursor_parallel_pools`。
- 不适用：`Quota.limit == 0` 或 `status == Unknown` → **不触发**。

### 1.2 限流触发

- 真实业务接口收到 HTTP 429 或识别为限流 → **立即**触发（不等下次轮询）。
- 实现：上游客户端钩子或 daemon 本地 IPC 上报。
- 权重高于阈值：quota 显示充裕也信任限流响应。
- **不通过高频轮询制造/探测 429**；无稳定上报通道前不实现主动探测。

### 1.3 采样入口

- `subswap` 无参：调用即采样一次（渐进式重判见 1.4）。
- `subswapd`（M4）：默认 60 秒一次。

### 1.4 Progressive decisions

The default entry queries accounts concurrently and re-evaluates the provider after every quota result (`fill_quotas_progressively` → `try_auto_swap_ready_provider`). While the active result is `Loading`, keep the current account. A `Failed` or `Stale` active result degrades without activation. After a `Ready` active result confirms a threshold breach or exhaustion, switch only when a usable target is also `Ready`; otherwise keep collecting results. A healthy active result always yields `NoOp`.

`AutoSwapProgress.activated_targets` prevents duplicate activation and `abandoned` prevents switching back within one invocation. With confirmed usable targets, a completed switch naturally yields `NoOp`; there is no intermediate jump through an unknown or depleted account.

The previous loading fallback caused healthy Codex and OpenCode accounts to alternate solely because a candidate query returned first. This is corrected in v1.11.1; timing and verification are recorded in [the incident](../troubleshooting/2026-09-29-codex-auto-swap-with-healthy-accounts.md).

## 2. Candidate selection

Apply these rules in order:

1. Stay within the same provider.
2. Preserve active `manual_only` accounts; exclude inactive `manual_only` accounts from every candidate path.
3. Respect manual hold: a successful manual `swap` / `login` suspends all automatic switching for `auto_swap.manual_hold_ms` (default 10 minutes), including confirmed exhaustion. The persisted provider hold survives CLI exits and daemon restarts. A value of `0` disables it.
4. Require a completed, non-stale query for the active account before evaluating its switching condition. Empty quotas, unknown status, and zero limits do not establish exhaustion. Cursor switches only when its parallel pools are all confirmed exhausted (or a separately reported hourly threshold is breached); a pool with unknown status is not confirmed exhausted.
5. Require a `Ready` candidate with usable quota, no hourly threshold breach, and no blocking exhausted window. Cursor accepts any usable parallel pool. Failed, loading, and stale candidates are excluded, including authentication failures and quota endpoint 429. `PolicyConfig.allow_unknown` remains an explicit internal override for unknown windows in a completed response; the default entry and daemon set it to false. It never permits loading, failed, or stale responses.
6. Among usable candidates, order by earliest `reset_at` (missing last), then usage ratio, account priority, and account ID. This ordering only selects a target after a valid trigger; it never replaces a healthy active account to gain more balance or an earlier reset.
7. If no usable candidate exists, try the all-exhausted fallback before degrading: it applies only when the active account is confirmed dead (at least one gating window `Exhausted` with limit > 0; a merely Warn/threshold-breached active account stays put) or when there is no active account. The pool admits only `Ready`, non-`manual_only`, non-active accounts whose gating windows are all confirmed (`limit > 0`, status known, account already unusable) with at least one known gating `reset_at`; failed, loading, stale, and unknown accounts never enter. Rank by earliest gating reset (tie: priority, then account ID) and swap only when the winner recovers strictly sooner than the active account (or the active recovery time is unknown / there is no active account); a non-empty pool whose winner is not sooner means stay (`NoOp`: current recovers soonest). If the pool is empty, return `Degraded` and re-evaluate after the next result or normal polling interval; do not add requests to force a decision.
8. The daemon retains its five-minute account cooldown and checks the current active identity immediately before activation. Discard decisions if the active identity changed or became manual-only.

`auto_swap.settle_grace_ms` and `PolicyConfig.settle_grace_ms` remain accepted for compatibility. Since v1.11.1, uncertain quotas always preserve the current account, regardless of account age or grace duration. The setting no longer changes the decision; confirmed exhaustion remains eligible for a meaningful switch. Manual hold continues to block even confirmed exhaustion.

## 2.5 风控与合规边界

- `query_quota` 只做低频采样；无参 `subswap` 是用户主动一次性采样。
- daemon 默认 60 秒轮询，失败退避；不得把周期调到秒级以下。
- 不绕过厂商并发、地域、账号共享、速率限制等政策。
- 新增 Provider 的 usage/refresh 须先写入 `docs/PROVIDER_KNOWLEDGE_BASE.md`（端点、频率、失败退避）。
- Active quota failure does not trigger extra requests or a swap. Preserve the account, report degraded status, and leave manual `subswap swap` available.

## 3. 降级到手动

下列情况下自动切换必须放弃并提示手动 `subswap swap`：

| 触发条件 | 现象 | 行为 |
|---|---|---|
| Active quota query failed or returned stale cache | Current exhaustion is unconfirmed | Preserve the account, even when another account is usable; log degraded status |
| 所有候选账号 `query_quota` 失败，且 active 未明确耗尽 | 不知道是否需要切换 | 不自动切换；提示 doctor + 手动 swap |
| All candidates are exhausted, regardless of reset times | No usable target now | Preserve the current account; wait for a normal refresh or use manual swap |
| Only unknown candidates remain | Target availability is unconfirmed | No automatic swap; manual swap remains available |
| 候选为 401/403、`needs re-login` 或凭据缺失 | 已知无法登录 | 无论是否有旧 quota 缓存都排除；提示重新登录或手动选择其他账号 |
| 切换过程中 `activate` 失败 | 文件写入冲突/keyring 故障 | 回滚快照；提示 doctor；不重试到其他账号 |
| 5 分钟内连续触发 ≥ 3 次 | 快速抖动 | 暂停自动切换 30 分钟；要求人工介入 |
| 15 分钟内同一目标账号被**切回 ≥ 2 次** | 振荡(A→B→A) | 同上：进 Degraded 30 分钟 |

**振荡检测为何不能只靠「5min 内 3 次」（2026-06-14）**：`cooldown`(默认 5min) == `FLAP_WINDOW`(5min) 时，冷却把回切卡到刚好 5min 一跳 → 任意 5min 窗口最多 2 次 → 永远够不到 3 → 刹车不触发（实测两废号间跳 60 次）。对策（`crates/daemon/src/state.rs`）：`swap_history` 存**目标账号+时间**，`detect_flap` 加振荡判定——`OSCILLATION_WINDOW`(15min，**必须明显 > cooldown**) 内同目标切回 ≥2 次即判抖动。快速 flap(5min×3) 与振荡(15min×同目标2) 取其一即进 Degraded。

> The flap brake is independent of candidate safety: ordinary quota uncertainty always preserves the active account, whether or not the brake is engaged.

降级输出建议：

```
[degraded] codex: active account alice quota fetch failed (timeout); cannot decide
```

人工介入：`subswap swap <id>`；跨 Provider 冲突用 `subswap swap <provider>/<id>`。

## 4. 状态机

```
       ┌─────────┐
       │  Idle   │◀──── 冷却结束 / 手动 reset
       └────┬────┘
            │ 触发（阈值或 429）
            ▼
       ┌─────────┐
       │ Picking │── 无候选 ──▶ Degraded (提示手动)
       └────┬────┘
            │ 选中目标
            ▼
       ┌─────────┐
       │Swapping │── 失败 ──▶ Degraded (回滚 + 提示)
       └────┬────┘
            │ 成功
            ▼
       ┌─────────┐
       │ Cooldown│── 5min ──▶ Idle
       └─────────┘
```

`Degraded` never authorizes activation. The default entry continues collecting quota and may re-evaluate when a later usable result arrives; the daemon retries on its normal polling schedule. Only the persistent flap/oscillation brake creates a timed provider suspension.

## 5. 通知

- 成功切换 / 进入 `Degraded`：本地系统通知 + 审计（Degraded 另标记状态文件，M4）。
- 通知后端（M4 之后）：可配置 Webhook。

## 5.5 Token 保活（daemon 兼职）

daemon 除自动切换外，负责**非活跃 Claude 账号 token 保活**：

- 每轮询周期（默认 60s）扫全部账号
- `expires_at - now < 1h` 且有 `refresh_token` → 刷新（写回 keyring，不动 `~/.claude/`）
- 失败仅 warn；不影响其它账号 / 自动切换
- 不暴露日常 CLI；用户无需 cron

动机：non-active 无人刷 token → 切过去立刻 401。Codex 不需要：access_token 都流过 `~/.codex/auth.json`，CLI 自刷新。

**Codex 自动切号已接通，但客户端不热读。** 默认入口与 `subswapd` 都会对 Codex 跑同一套 `decide` → `activate`（写 live `auth.json`）。已运行的官方 Codex **不会**因此换成新号，须重启；macOS 默认不拉起 daemon 时后台也不会自动切。细则与禁令见 [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「切换生效边界」与 [troubleshooting/2026-09-11](../troubleshooting/2026-09-11-codex-swap-requires-restart.md)。

## 6. 配置项（config.toml）

字段语义与默认以 [CONFIG.md](../CONFIG.md) / `defaults.rs` 为准。结构示意：

```toml
[auto_swap]
enabled = true
# threshold = 0.99             # Default: defaults::AUTO_SWAP_THRESHOLD
cooldown_ms = 300000
# settle_grace_ms = 60000      # Legacy compatibility; no decision effect
manual_hold_ms = 600000

[daemon]
poll_interval_ms = 60000
```

## 7. 测试要点

- 单元：`AutoSwapPolicy` 给定 Quota 列表，断言挑选结果。
- 集成：mock Provider 模拟 quota 失败、429、Exhausted 等，验证降级。
- 鉴权失败候选：带旧缓存的 401/403、`needs re-login`、凭据缺失不得成自动候选。
- `manual_only`: active remains selected; inactive is excluded from every automatic candidate path.
- Across all providers: candidate-first completion, timeout/429/401, stale exhaustion, and exhausted targets must never produce a swap; confirmed exhaustion plus a ready usable target must still swap.
- All-exhausted fallback: active 5h exhausted + every other account confirmed exhausted → swap to the one with the earliest gating reset; tie with the active account → stay; failed/loading/stale/unknown or `manual_only` accounts in the pool → excluded (`Degraded` when the pool is empty); Warn-only (not exhausted) active + all others exhausted → stay.
- 端到端：双账号 + mock HTTP，跑 `subswap` 看 keyring 与 client_targets 同步。

<!-- 该文档整理/压缩于 2026-09-05 -->

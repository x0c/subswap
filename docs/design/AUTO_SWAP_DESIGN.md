# 自动切换设计

## 0. 核心不变量

**手动 `subswap swap` 命令永远独立于额度查询。** 即使 quota 接口、网络、凭证密钥任一不可用，
手动切换都必须能跑通。本文档描述的自动切换是在「条件具备时锦上添花」的能力，**不能成为切换的唯一通路**。

## 1. 触发策略（阈值 + 限流双触发）

### 1.1 阈值触发

- 默认阈值：由 `crates/core/src/defaults.rs::AUTO_SWAP_THRESHOLD` 定义，运行时可由 `config.toml` 覆盖。
- 适用窗口：Provider 返回的所有窗口（Claude 的 5h / 7d、Codex 的月度等）任一命中即触发。
- 不适用条件：`Quota.limit == 0` 或 `status == Unknown` 时**不触发**（无法判断，保守不动）。

### 1.2 限流触发

- 当 Provider 调用真实业务接口收到 HTTP 429 或被识别为限流响应时，**立即**触发（不等下次轮询）。
- 实现方式：上游客户端（Codex CLI / Claude CLI）的调用钩子或 daemon 提供本地 IPC 接受上报。
- 限流触发权重高于阈值触发：即使 quota 显示充裕，限流响应也应当被信任。
- **不通过高频轮询制造/探测 429**：429 触发只能来自真实客户端上报或明确的本地 IPC。
  在没有稳定上报通道前，不实现“主动探测限流”。

### 1.3 采样入口

- `subswap` 无参默认入口：调用即采样一次，查询所有已登记账号额度并按策略自动切换（渐进式重判见 1.4）。
- `subswapd` daemon（M4）：默认 60 秒采样一次。

### 1.4 默认入口的渐进式重判（每收到一份额度重判一次，单调升级）

默认入口的额度是**边查边回**（每账号一个 `tokio::spawn` + mpsc，按返回顺序逐份回填）。
关键约束：**不能查到第一份额度就把决策锁死**。否则更优候选还在 loading 时，会先切到一个
「逃生/兜底候选」（第 2 节第 6~8 条的 loading/失败兜底），等更优候选额度落地却已错过，
表现为：连跑两次 `subswap` 结果不同、甚至停在一个已耗尽的号上不动。

正确行为（`crates/cli/src/cmd/default.rs::fill_quotas_progressively` →
`try_auto_swap_ready_provider`）：**每收到一份 quota 更新就对该 provider 重跑一次 `decide`**，
有更优目标就升级过去——一次 `subswap` 内自我纠正，无需用户再跑一遍。

单调收敛靠三点，缺一会抖动：
1. `decide` 只在当前 active 确实不行（耗尽/超阈值/loading/失败）时才返回 `Swap`；切到真正可用号后自然 `NoOp`。
2. `AutoSwapProgress.activated_targets`：本次运行已切到的目标不重复 `activate`（避免重写凭证）。
3. `AutoSwapProgress.abandoned`：本次运行主动离开过的账号不再切回，「只升级、不回头」，杜绝 A→B→A。

注意与 settle-grace（2 节 8.5）的配合：刚激活号只挡 loading/失败这类**不确定**状态；
**已耗尽是确定状态**，照样会被升级走，所以不会把用户卡在耗尽号上。

## 2. 候选账号筛选

按顺序应用：

1. **同 Provider 内**：自动切换不跨 Provider（用户可能并非两边都有付费）。
2. **可用性**：优先选择所有窗口都未达到自动切换阈值、且没有 `Exhausted` 窗口的账号。
3. **冷却期**：刚被切走的账号默认 5 分钟内不再选回，避免触发→回切抖动。
4. **优先级排序**：
   1. `usage_ratio` 升序（剩余多的优先）；
   2. `Account.priority` 升序（用户配置的偏好）；
   3. `id` 字典序（稳定 tie-break）。
5. **无可用候选时的重置兜底**：如果其他账号也已超阈值 / `Exhausted`，但阻塞窗口都带有
   `reset_at`，允许切到最早恢复可用的账号。多窗口账号取所有阻塞窗口 `reset_at` 的最大值，
   避免 5h 刚刷新但 7d 仍阻塞时马上抖动；如果当前激活账号本身就是最早恢复的账号，则保持不动等待刷新。
6. **查询失败候选兜底**：当前账号已明确耗尽、且没有已知可用候选时，允许切到 `query_quota` 失败的其他账号；
   查询失败不代表账号不可用，继续停留在已耗尽账号则一定无法承接流量。
7. **active 查询失败兜底**：active 的 `query_quota` 失败时，如果存在额度明确可用的其他账号则切走；没有明确
   可用候选时才降级，禁止从未知状态盲切到另一个未知状态。
8. **active 查询仍在加载兜底**：CLI 渐进刷新期间，如果 active 仍在 loading，而候选账号已经返回明确可用 quota，
   立即切换；如果尚无明确可用候选，则继续等待后续 quota 更新，不提前定案。
8.5. **新激活沉淀宽限（settle grace）**：account 刚成为 active（`last_used_at` 距今 < `auto_swap.settle_grace_ms`，
   默认 60s；手动 swap 与自动切换都刷新 `last_used_at`）时，**不因第 7、8 条这类「loading / 查询失败」的不确定
   状态把它切走**——直接 `NoOp` 等待 quota 沉淀。动机：否则用户手动 `swap` 到某账号后，仅仅运行一次 `subswap`
   （默认入口会跑同一套 `decide`）或被 daemon 撞上 quota 冷启动正在 loading，就会被立刻顶回别的账号，违背显式选择。
   **只挡不确定状态**：账号若已明确达到 threshold / `Exhausted`（第 2 步确定性数据），即使在宽限期内仍按正常逻辑切走。
   宽限期需覆盖一次冷 quota 查询（含重试退避）的耗时。改默认值只动 `crates/core/src/defaults.rs::AUTO_SWAP_SETTLE_GRACE_MS`。
9. **`manual_only` 强制边界**：`Account.extra.manual_only == true` 的账号只能由用户手动激活；
   active 命中时立即 `NoOp`，即使 quota 仍在 loading / 查询失败也不自动切走；inactive 时从所有候选路径排除。
   Claude 自定义 API 使用此语义，因为它没有可比较的订阅 quota。
10. **执行前重验 active**：daemon 的 quota 查询期间用户可能手动切换账号；执行自动切换前必须重新读取 registry。
    只有当前 active 仍等于决策快照中的 active、且当前 active 不是 `manual_only` 时才能执行，否则丢弃过期决策。

## 2.5 风控与合规边界

subswap 的目标是减少重复登录和人工切换，不是规避厂商限制。实现上必须遵守：

- `query_quota` 只做低频状态采样；无参 `subswap` 是用户主动触发的一次性采样。
- daemon（M4）默认 60 秒轮询，失败后必须退避；不得把轮询周期调到秒级以下。
- 不绕过厂商的并发、地域、账号共享、速率限制等使用政策。
- 任何新增 Provider 的 usage/refresh 请求都必须先写入 `docs/PROVIDER_KNOWLEDGE_BASE.md`，说明端点、频率和失败退避策略。
- active quota 查询失败时不补打额外请求；有明确可用候选则切走，否则 Degraded 并提示手动 `subswap swap`。

## 3. 降级到手动

下列情况下，**自动切换必须主动放弃并明确提示用户手动 `subswap swap`**：

| 触发条件 | 现象 | 行为 |
|---|---|---|
| 当前账号 `query_quota` 失败且无明确可用候选 | 不知道是否超额 | 不自动切换；记录 warn 日志；CLI 提示 |
| 所有候选账号 `query_quota` 失败，且 active 未明确耗尽 | 不知道是否需要切换 | 不自动切换；提示 doctor + 手动 swap |
| 所有候选 `status == Exhausted` 且无 `reset_at` | 不知道何时恢复 | 不切；提示用户等重置时间或加账号 |
| 候选只剩 `Unknown` | 不确定能否承接 | 默认**不切**；可通过 `--allow-unknown` 强制 |
| 切换过程中 `activate` 失败 | 文件写入冲突/keyring 故障 | 回滚快照；提示 doctor；不重试到其他账号 |
| 5 分钟内连续触发 ≥ 3 次 | 快速抖动 | 暂停自动切换 30 分钟；要求人工介入 |
| 15 分钟内同一目标账号被**切回 ≥ 2 次** | 振荡(A→B→A) | 同上：进 Degraded 30 分钟 |

**振荡检测为何不能只靠「5min 内 3 次」（2026-06-14 修复的真 bug）**：`cooldown`(默认 5min) ==
`FLAP_WINDOW`(5min) 时，冷却把每个号的回切卡到刚好 5min 一跳，任意 5min 窗口最多数到 2 次 →
永远够不到 3 → 刹车一次都不触发，A↔B 无限横跳（实测 codex 在两个全耗尽/401 的废号间从 5/29 跳了 60 次）。
对策（`crates/daemon/src/state.rs`）：`swap_history` 改存**目标账号+时间**，`detect_flap` 增加
**振荡判定**——`OSCILLATION_WINDOW`(15min，**必须明显 > cooldown**) 内同一目标被切回 ≥2 次即判抖动。
快速 flap(5min×3) 与振荡(15min×同目标2) 取其一即进 Degraded。

> 注意：刹车只「停止瞎切」，不保证停在**最优**号。当 active 是失败号且无可用候选时，决策返回
> Degraded 就地不动，可能赖在 401 废号上而非「最快恢复」的号——那是候选筛选的进一步优化(未做)，
> 与防抖刹车正交。

降级时 CLI/daemon 输出格式建议：

```
[degraded] codex: active account alice quota fetch failed (timeout); cannot decide
```

用户需要人工介入时，直接执行 `subswap swap <id>`；若 id 跨 Provider 冲突，用
`subswap swap <provider>/<id>`。

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

`Degraded` 是显式终态：本次 `subswap` 不再尝试切换；daemon 场景（M4）会暂停该 Provider
的自动切换，直到冷却结束或进程重启。理由：连续失败时继续盲切换风险大于收益。

## 5. 通知

- 成功切换：本地系统通知 + 审计日志。
- 进入 `Degraded`：本地系统通知 + 标记状态文件（M4 daemon 状态显示）。
- 通知后端（M4 之后）：可配置 Webhook，方便接入飞书/Slack/邮件。

## 5.5 Token 保活（daemon 兼职）

daemon 启动后除了自动切换，还负责**非活跃 Claude 账号的 token 保活**：

- 每个轮询周期（默认 60s）扫描全部账号
- 任一账号 `expires_at - now < 1h` 且 keyring 中有 `refresh_token` → 触发刷新（写回 keyring，不动 `~/.claude/`）
- 失败仅 warn，不影响其它账号 / 自动切换主流程
- 这是「应用自己后台干的事」，不暴露给日常 CLI 工作流，用户不需要写 cron。

设计动机：non-active 账号没人帮它刷 token，切过去时 token 已过期 → Claude CLI 立刻 401。

Codex **不需要**这个机制：所有账号的 access_token 最终都流过 `~/.codex/auth.json`，Codex CLI 自己持续刷新。

## 6. 配置项（config.toml）

```toml
[auto]
enabled = true                  # 总开关
# threshold = <0.0~1.0>         # 阈值触发上限，默认见 defaults.rs
cooldown_seconds = 300          # 切换冷却
# settle_grace_ms = 60000       # 新激活账号沉淀宽限：此窗口内不因 loading/查询失败被切走
poll_interval_seconds = 60      # daemon 轮询周期
allow_unknown = false           # 是否允许选择 status=Unknown 的候选
max_flap_per_5min = 3           # 抖动上限，超过进入 Degraded

[auto.providers.codex]          # 可按 Provider 覆写
# threshold = <0.0~1.0>
```

## 7. 测试要点

- 单元：`AutoSwapPolicy` 给定一组 Quota 列表，断言挑选结果。
- 集成：mock Provider 模拟 quota 失败、429、Exhausted 等组合，验证降级路径。
- `manual_only`：验证 active 时不自动切走、inactive 时不成为已知可用 / 查询失败 / reset 兜底候选。
- 端到端：本地双账号 + mock HTTP server，跑 `subswap` 看 keyring 与 client_targets 是否同步。

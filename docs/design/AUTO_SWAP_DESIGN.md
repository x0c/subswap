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

- `subswap` 无参默认入口：调用即采样一次，查询所有已登记账号额度并按策略自动切换。
- `subswapd` daemon（M4）：默认 60 秒采样一次。

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
| 5 分钟内连续触发 ≥ 3 次 | 抖动 | 暂停自动切换 30 分钟；要求人工介入 |

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
poll_interval_seconds = 60      # daemon 轮询周期
allow_unknown = false           # 是否允许选择 status=Unknown 的候选
max_flap_per_5min = 3           # 抖动上限，超过进入 Degraded

[auto.providers.codex]          # 可按 Provider 覆写
# threshold = <0.0~1.0>
```

## 7. 测试要点

- 单元：`AutoSwapPolicy` 给定一组 Quota 列表，断言挑选结果。
- 集成：mock Provider 模拟 quota 失败、429、Exhausted 等组合，验证降级路径。
- 端到端：本地双账号 + mock HTTP server，跑 `subswap` 看 keyring 与 client_targets 是否同步。

# 自动切换设计

## 0. 核心不变量

**手动 `subswap swap` 命令永远独立于额度查询。** 即使 quota 接口、网络、凭证密钥任一不可用，
手动切换都必须能跑通。自动切换是「条件具备时锦上添花」，**不能成为切换的唯一通路**。

## 1. 触发策略（阈值 + 限流双触发）

### 1.1 阈值触发

- 默认阈值：由 `crates/core/src/defaults.rs::AUTO_SWAP_THRESHOLD` 定义，运行时可由 `config.toml` 覆盖。
- 适用窗口：只看小时级（当前可可靠识别 `FiveHour`）。Claude 7d、Codex 月度、OpenCode weekly/monthly 等长窗口即使接近阈值也不触发。OpenCode `rolling`（约 5 小时）映射为 `FiveHour`，走阈值触发。
- 硬阻断：对 **Claude / Codex 等叠加上限**，任一参与自动切换的窗口 `Exhausted` 即触发/阻断。
  **Cursor 例外**：`1st`、**Credits**、**API** 是并行可用池——任一池仍有余量即可承接；
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

### 1.4 默认入口的渐进式重判（每收到一份额度重判一次，单调升级）

额度边查边回（每账号 `tokio::spawn` + mpsc）。**不能查到第一份就把决策锁死**，否则更优候选仍 loading 时会先切到逃生/兜底候选（§2 第 6~8 条），表现为连跑两次结果不同、甚至停在已耗尽号。

正确行为（`crates/cli/src/cmd/default.rs::fill_quotas_progressively` →
`try_auto_swap_ready_provider`）：**每收到一份 quota 对该 provider 重跑 `decide`**，有更优目标就升级。

单调收敛三点（缺一会抖动）：
1. `decide` 仅在 active 确实不行（耗尽/超阈值/loading/失败）时返回 `Swap`；切到可用号后 `NoOp`。
2. `AutoSwapProgress.activated_targets`：本次已切到的目标不重复 `activate`。
3. `AutoSwapProgress.abandoned`：本次主动离开过的账号不再切回（只升级、不回头）。

与 settle-grace（§2 条 8.5）配合：刚激活号只挡 loading/失败等**不确定**状态；**已耗尽是确定状态**，照样升级走。

## 2. 候选账号筛选

按顺序应用：

1. **同 Provider 内**：不跨 Provider。
2. **可用性**：优先小时级未达阈值且无 `Exhausted` 窗口的账号；长窗口达阈值但未耗尽仍可作候选。
3. **冷却期**：刚被切走的账号默认 5 分钟内不再选回。
4. **优先级排序**（`compare_candidates`）：
   1. 窗口最快 `reset_at` 升序（缺失视为最晚）；
   2. `usage_ratio` 升序；
   3. `Account.priority` 升序；
   4. `id` 字典序。
   此排序只影响触发后挑哪个；不改变触发条件（仍是小时级阈值/429/loading/失败兜底）。
5. **无可用候选时的重置兜底**：其他账号也超阈值 / `Exhausted`，但阻塞窗口都带 `reset_at` → 切到最早恢复者。多窗口取所有阻塞窗口 `reset_at` 最大值；若当前 active 已是最早恢复者则不动。
6. **查询失败候选兜底**：当前已明确耗尽、无已知可用候选时，允许切到因网络/超时/429 等导致 `query_quota` 失败的账号。**401/403、`needs re-login`、凭据缺失例外：即使有旧 quota 缓存也必须排除。**
7. **active 查询失败兜底**：存在额度明确可用的其他账号则切走；无明确可用候选才降级；禁止未知→未知盲切。
8. **active 仍在加载兜底**：有明确可用候选则立即切；否则继续等待，不提前定案。
8.5. **新激活沉淀宽限（settle grace）**：`last_used_at` 距今 < `auto_swap.settle_grace_ms`（默认 60s；手动/自动切换都刷新）时，**不因第 7、8 条 loading/查询失败切走** → `NoOp`。**只挡不确定状态**：已达 threshold / `Exhausted` 仍正常切走。宽限期须覆盖一次冷 quota 查询（含重试）。改默认只动 `crates/core/src/defaults.rs::AUTO_SWAP_SETTLE_GRACE_MS`。
9. **`manual_only`**：`Account.extra.manual_only == true` → active 立即 `NoOp`（即使 loading/失败也不切走）；inactive 从所有候选路径排除。Claude 自定义 API 用此语义。
10. **执行前重验 active**：daemon 执行前重读 registry；仅当当前 active 仍等于决策快照且非 `manual_only` 才执行，否则丢弃过期决策。

## 2.5 风控与合规边界

- `query_quota` 只做低频采样；无参 `subswap` 是用户主动一次性采样。
- daemon 默认 60 秒轮询，失败退避；不得把周期调到秒级以下。
- 不绕过厂商并发、地域、账号共享、速率限制等政策。
- 新增 Provider 的 usage/refresh 须先写入 `docs/PROVIDER_KNOWLEDGE_BASE.md`（端点、频率、失败退避）。
- active quota 失败不补打额外请求；有明确可用候选则切走，否则 Degraded + 提示手动 `subswap swap`。

## 3. 降级到手动

下列情况下自动切换必须放弃并提示手动 `subswap swap`：

| 触发条件 | 现象 | 行为 |
|---|---|---|
| 当前账号 `query_quota` 失败且无明确可用候选 | 不知道是否超额 | 不自动切换；记录 warn 日志；CLI 提示 |
| 所有候选账号 `query_quota` 失败，且 active 未明确耗尽 | 不知道是否需要切换 | 不自动切换；提示 doctor + 手动 swap |
| 所有候选 `status == Exhausted` 且无 `reset_at` | 不知道何时恢复 | 不切；提示用户等重置时间或加账号 |
| 候选只剩 `Unknown` | 不确定能否承接 | 默认**不切**；可通过 `--allow-unknown` 强制 |
| 候选为 401/403、`needs re-login` 或凭据缺失 | 已知无法登录 | 无论是否有旧 quota 缓存都排除；提示重新登录或手动选择其他账号 |
| 切换过程中 `activate` 失败 | 文件写入冲突/keyring 故障 | 回滚快照；提示 doctor；不重试到其他账号 |
| 5 分钟内连续触发 ≥ 3 次 | 快速抖动 | 暂停自动切换 30 分钟；要求人工介入 |
| 15 分钟内同一目标账号被**切回 ≥ 2 次** | 振荡(A→B→A) | 同上：进 Degraded 30 分钟 |

**振荡检测为何不能只靠「5min 内 3 次」（2026-06-14）**：`cooldown`(默认 5min) == `FLAP_WINDOW`(5min) 时，冷却把回切卡到刚好 5min 一跳 → 任意 5min 窗口最多 2 次 → 永远够不到 3 → 刹车不触发（实测两废号间跳 60 次）。对策（`crates/daemon/src/state.rs`）：`swap_history` 存**目标账号+时间**，`detect_flap` 加振荡判定——`OSCILLATION_WINDOW`(15min，**必须明显 > cooldown**) 内同目标切回 ≥2 次即判抖动。快速 flap(5min×3) 与振荡(15min×同目标2) 取其一即进 Degraded。

> 刹车只停瞎切，不保证停在最优号。active 是失败号且无可用候选时 Degraded 就地不动——与防抖正交，候选筛选进一步优化未做。

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

`Degraded` 是显式终态：本次 `subswap` 不再尝试；daemon（M4）暂停该 Provider 自动切换，直到冷却结束或进程重启。连续失败时盲切风险大于收益。

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

## 6. 配置项（config.toml）

字段语义与默认以 [CONFIG.md](../CONFIG.md) / `defaults.rs` 为准。结构示意：

```toml
[auto]
enabled = true                  # 总开关
# threshold = <0.0~1.0>         # 权威：defaults::AUTO_SWAP_THRESHOLD
cooldown_seconds = 300          # 切换冷却
# settle_grace_ms = ...         # 新激活沉淀宽限；权威：AUTO_SWAP_SETTLE_GRACE_MS
poll_interval_seconds = 60      # daemon 轮询周期
allow_unknown = false           # 是否允许 status=Unknown 候选
max_flap_per_5min = 3           # 抖动上限，超过进 Degraded

[auto.providers.codex]          # 可按 Provider 覆写
# threshold = <0.0~1.0>
```

## 7. 测试要点

- 单元：`AutoSwapPolicy` 给定 Quota 列表，断言挑选结果。
- 集成：mock Provider 模拟 quota 失败、429、Exhausted 等，验证降级。
- 鉴权失败候选：带旧缓存的 401/403、`needs re-login`、凭据缺失不得成自动候选。
- `manual_only`：active 不自动切走；inactive 不进已知可用 / 查询失败 / reset 兜底。
- 端到端：双账号 + mock HTTP，跑 `subswap` 看 keyring 与 client_targets 同步。

<!-- 该文档整理/压缩于 2026-09-05 -->

# subswap 运行时配置（config.toml）

`subswap` 与 `subswapd` 的数值调优参数从一个文件加载：

```
Linux:   ~/.config/subswap/config.toml
macOS:   ~/Library/Application Support/dev.subswap.subswap/config.toml
Windows: %APPDATA%\subswap\subswap\config\config.toml
```

文件可以不存在（用 `crates/core/src/defaults.rs` 编译期默认）；可以只写部分字段（未写沿用默认）。

## 应用目录覆盖（高级）

`SUBSWAP_HOME` 把配置、账号仓库、运行状态和缓存收口到一个目录（三平台集成测试 / 便携运行）。不是 `config.toml` 字段，进程启动时读取；必须是绝对路径，否则启动失败。

| 逻辑目录 | 设置后的实际位置 | 主要内容 |
|---|---|---|
| 配置 | `<SUBSWAP_HOME>/config/` | `config.toml`、`registry.toml` |
| 数据 | `<SUBSWAP_HOME>/data/` | `credentials.json`、审计与 daemon 日志、隔离运行目录 |
| 状态 | `<SUBSWAP_HOME>/data/state/` | 切换快照、daemon PID、Provider 跨进程协调状态 |
| 缓存 | `<SUBSWAP_HOME>/cache/` | `quota_cache.json` |

目录不存在时自动创建。同一套账号状态下，CLI 与独立启动的 daemon 必须使用相同的 `SUBSWAP_HOME`；由 CLI 拉起的 daemon 会自然继承当前值。

只管理 subswap 自己的目录，不搬迁 Codex / Claude / Kimi / Cursor 原生登录位置。完整测试隔离还要分别重定向各原生客户端目录、Cursor 数据库和 macOS Claude / Cursor 测试钥匙串，见 [OPERATIONS_GUIDE.md](OPERATIONS_GUIDE.md)「三平台测试隔离」。Cursor 可用 `SUBSWAP_CURSOR_STATE_DB_PATH` 指向临时 `state.vscdb`，macOS 命令行钥匙串用 `SUBSWAP_CURSOR_KEYCHAIN_PATH`；两者都只接受绝对路径，是测试/便携覆盖，不是普通运行时调优项。

## 热加载

- `subswapd` 每轮循环开头读一次；改了文件下一轮生效（活跃时约 ≤ 60s，空闲时最长约 30 分钟；已知 reset 唤醒可能更早）。
- `subswap` CLI 每次启动读一次；下次执行生效。
- **解析失败不会拖挂 daemon**：保留上一次成功加载的值，日志打 `reload config failed; keeping previous values`。
- TOML **未识别字段**会让解析失败（`deny_unknown_fields`），便于早发现 typo。

## 完整字段表

| 字段 | 默认值 | 单位 | 说明 |
|---|---|---|---|
| `auto_swap.threshold` | `defaults::AUTO_SWAP_THRESHOLD` | 0.0~1.0 | 小时级窗口 `used/limit ≥` 此值 → 自动切换；7d/月度等长窗口只在明确耗尽时阻断 |
| `auto_swap.cooldown_ms` | `300000` | 毫秒 | 切换后该账号冷却期，daemon 内不再选回 |
| `auto_swap.settle_grace_ms` | `60000` | ms | Legacy compatibility setting; since v1.11.1 uncertain quotas always preserve the active account, regardless of this duration |
| `auto_swap.manual_hold_ms` | `600000` | 毫秒 | 手动 `swap` / `login` 后该 provider 暂停一切自动切换（含确定性额度切换），避免把显式选择掰回去；`0` 关闭 |
| `quota.warn_pct` | `90.0` | 0~100 | CLI 显示 `warn` 的阈值；不参与切换决策 |
| `quota.exhausted_pct` | `100.0` | 0~100 | CLI 显示 `full` 的阈值；不参与切换决策 |
| `quota.fetch_timeout_ms` | `20000` | 毫秒 | 单次 quota 查询 attempt 的超时；需盖住 Codex app-server（≤20s）与 Kimi 401 自愈；超时后按 `quota.fetch_retries` 决定是否重试 |
| `quota.fetch_retries` | `1` | 次 | quota 查询失败后额外重试次数；最多 5 次，401/403/429 不重试 |
| `quota.fetch_retry_delay_ms` | `500` | 毫秒 | 首次 quota 重试等待时间；后续按 500ms、1s、2s、4s、8s 指数退避 |
| `quota.min_refresh_interval_ms` | `90000` | 毫秒 | 同一账号两次真实 quota 查询的最小间隔；缓存比它新就复用、不打端点。daemon 与 CLI 共用 `quota_cache.json`，节流对两条路径同时生效 |
| `quota.failure_backoff_max_ms` | `900000` | 毫秒 | 连续查询失败时的退避上限；退避从 `min_refresh_interval_ms` 起按连续失败次数翻倍并封顶到此值。失败结果不进缓存，没有它时查不出的账号会被每轮重打，反而比健康账号更高频 |
| `quota.active_refresh_interval_ms` | `180000` | 毫秒 | 后台对**当前激活账号**的目标刷新间隔。实际请求仍受 `min_refresh_interval_ms`、Provider 专属下限与协调器 `next_attempt_at` 约束 |
| `quota.parked_refresh_interval_ms` | `900000` | 毫秒 | 后台对非激活、未确认耗尽账号的目标刷新间隔 |
| `quota.exhausted_refresh_interval_ms` | `900000` | 毫秒 | 非激活、已确认耗尽账号的后台刷新间隔；低频检查只用于发现提前发放，不把计时器到期当成额度恢复 |
| `quota.decision_max_age_ms` | `300000` | 毫秒 | 自动决策可接受的最大数据年龄；更老的额度只展示为 stale，不能作为切换触发或候选依据 |
| `quota.candidate_refresh_margin_pct` | `15.0` | 0~100 | 激活账号进入「阈值 - margin」区间时提前确认候选；不改变切换阈值 |
| `quota.candidate_refresh_limit` | `2` | 个 | 接近阈值时最多提前刷新的非 manual_only 候选数；其余候选仍按普通节奏刷新 |
| `quota.refresh_jitter_pct` | `10.0` | 0~100 | 自适应间隔的正向抖动上限；只增加等待，不会早于基础间隔、Provider 下限或服务端截止时间 |
| `quota.max_concurrent_fetches` | `4` | 个 | 同一进程同时进行真实采集的账号上限；跨进程另有协调器槽位限制 |
| `quota.max_concurrent_per_provider` | `1` | 个 | 同一 Provider 同时进行的真实采集上限；默认串行以保护共享限流桶 |
| `quota.reset_refresh_delay_ms` | `2000` | 毫秒 | 任意已知 gating 窗口倒计时（健康账号也算）到期后延迟多久显式刷新受影响账号；同一账号同一时刻的多个窗口合并为一次查询 |
| `token.refresh_slack_ms` | `300000` | 毫秒 | Claude token 距过期此值内 → 触发预刷新 |
| `daemon.poll_interval_ms` | `60000` | 毫秒 | 活跃时轮询间隔 |
| `daemon.idle_threshold_ms` | `1800000` | 毫秒 | provider probe 文件 mtime 距今超过此值视为「用户没在用」 |
| `daemon.idle_poll_interval_ms` | `1800000` | 毫秒 | 空闲时轮询间隔 |
| `codex.usage_cache_max_age_ms` | `600000` | 毫秒 | wham/usage 字段漂移时，允许使用本地 last_usage 缓存的最大年龄 |
| `codex.reset_details_timeout_ms` | `1000` | ms | Maximum wait for optional parked-account reset-credit details; `0` skips the extra request. Active accounts only reuse inline official details, so optional HTTP requests never delay their main quotas. |
| `claude.usage_min_refresh_interval_ms` | `180000` | ms | Minimum interval between Claude network usage queries, also bounded by `quota.min_refresh_interval_ms`. A per-account persistent reservation coordinates CLI and daemon; fresh official snapshots add no request. |
| `claude.usage_rate_limit_backoff_ms` | `1800000` | ms | Conservative minimum wait after Claude 429. A later server `Retry-After` deadline wins, even beyond the generic 15-minute cap. |

## 自适应后台采集

`subswapd` 不再对所有账号每轮同频查询，而是按账号状态与真实观测时间调度：

- **节奏**：当前激活账号约 3 分钟，停放与已确认耗尽账号约 15 分钟；在基础间隔上叠加 `0~refresh_jitter_pct` 的**正向**抖动，避免多账号同一秒打向端点。抖动只会延后，不会提前，也不会绕开 `min_refresh_interval_ms`、Provider 专属下限（如 Claude 三分钟）或协调器记录的 `next_attempt_at` / 服务端 `Retry-After`。
- **前台不降级**：无参 `subswap`、写操作后的状态面仍可主动检查；它们只是复用同一个共享协调器，真实请求仍服从最小间隔、并发槽位与服务端截止时间。GUI/`status --json` 保持只读，不触发采集。
- **新鲜度**：只有不晚于 `decision_max_age_ms` 的真实观测才能作为自动切换依据。超过年龄的旧数据仍可展示，但状态是 `stale`，不会因为「本轮调度没查」变成 `ready`。
- **近阈值候选**：激活账号进入「`auto_swap.threshold` - `candidate_refresh_margin_pct`」区间（或已确认耗尽）时，最多提前确认 `candidate_refresh_limit` 个非 `manual_only`、且缺少新鲜数据的候选；不会整池刷新。已有新鲜数据的候选不重复请求。
- **已知 reset**：任何**已展示的 gating 窗口倒计时**到期（`ResetCredits` 除外）都应按 `reset + reset_refresh_delay_ms` 显式刷新受影响账号，健康账号（例如 5h 只用了 40%）同样如此，不能等 3/15 分钟普通节奏。同一账号多个窗口共享同一 reset 时间只查询一次；不同窗口的后续 reset 继续依次调度。叠加/并行池的有效恢复时间语义只用于自动切换的恢复排序，不再决定是否安排 reset 刷新。调度会先读取协调器记录，并在升级前缓存尚未生成协调记录时用其原始观测作为种子；唤醒仍服从 `min_refresh_interval_ms`、Provider 下限与服务端 `Retry-After`。已处理过的同一 reset 不会因服务端仍返回旧时间而重复查询；计时器到期本身不证明额度恢复，只有采集返回的真实读数才会更新状态。
- **普通轮询仍保留**：没有 reset 信息或出现提前发放时，15 分钟的低频检查继续兜底；耗尽账号不会每轮重查。空闲模式（probe 文件超过 `daemon.idle_threshold_ms` 未变化）保留既有的空闲轮询节奏，只允许已知 reset 唤醒提前。
- **并发**：`max_concurrent_fetches` 限制同进程并发采集总数，`max_concurrent_per_provider` 限制单 Provider 并发；两者都不绕过跨进程协调器。

## 示例：放慢 daemon

```toml
[daemon]
poll_interval_ms = 120000           # 活跃 2 分钟一轮
idle_threshold_ms = 600000          # 10 分钟无活动就视为空闲
idle_poll_interval_ms = 1800000     # 空闲 30 分钟一轮
```

## 风控约束

- `auto_swap.threshold` 设过低（如 0.5）→ 容易飘出 `Degraded`（两个号都过阈值，policy 不再切，需要手动 swap）。
- `quota.fetch_retries` 最大按 5 次生效；401/403/429 不重试，其余失败按 `quota.fetch_retry_delay_ms` 指数退避。
- `quota.fetch_timeout_ms` 不要压到 Codex app-server / Kimi 自愈完成时间以下，否则会误报 `timeout after N attempts`。
- Reducing quota or Claude-specific intervals increases contention with native clients. Historical per-minute observations are not a guaranteed Anthropic allowance; account-specific and hour-scale limits have been reported. Claude's durable retry deadline remains authoritative even if the shared cache is overwritten by another process.
- `daemon.poll_interval_ms` 是**唤醒**间隔，不等于每账号请求间隔；真实采集由 `quota.active_refresh_interval_ms` / `parked_refresh_interval_ms` / `exhausted_refresh_interval_ms` 与协调器下限共同决定。即便如此也不要把唤醒调到秒级。
- `daemon.idle_*` 的初衷是「用户真没在用 AI 时别打 quota 请求」，**不要**通过缩小 `idle_threshold_ms` 把空闲化掉；空闲时自适应到期不会提前唤醒 daemon，只有已知 reset 可以。

详见 docs/design/ARCHITECTURE.md §5.6「风控边界」。

## 改了之后怎么验证

```bash
# 看看 daemon 实际用的间隔（DEBUG 级日志会打 sleeping until next cycle）
SUBSWAPD_LOG=debug nohup subswapd >/tmp/subswapd.log 2>&1 &
tail -f /tmp/subswapd.log | grep "sleeping until next cycle"

# 看 CLI 当前生效的策略阈值（policy debug 日志）
RUST_LOG=subswap_core::auto_policy=debug subswap
```

<!-- 该文档整理/压缩于 2026-09-05 -->

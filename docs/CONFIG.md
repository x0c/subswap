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

- `subswapd` 每轮循环开头读一次；改了文件下一轮（≤ 60s）生效。
- `subswap` CLI 每次启动读一次；下次执行生效。
- **解析失败不会拖挂 daemon**：保留上一次成功加载的值，日志打 `reload config failed; keeping previous values`。
- TOML **未识别字段**会让解析失败（`deny_unknown_fields`），便于早发现 typo。

## 完整字段表

| 字段 | 默认值 | 单位 | 说明 |
|---|---|---|---|
| `auto_swap.threshold` | `defaults::AUTO_SWAP_THRESHOLD` | 0.0~1.0 | 小时级窗口 `used/limit ≥` 此值 → 自动切换；7d/月度等长窗口只在明确耗尽时阻断 |
| `auto_swap.cooldown_ms` | `300000` | 毫秒 | 切换后该账号冷却期，daemon 内不再选回 |
| `auto_swap.settle_grace_ms` | `60000` | 毫秒 | 账号刚激活后此窗口内不因 quota loading / 查询失败被自动切走，避免顶掉手动选择 |
| `quota.warn_pct` | `90.0` | 0~100 | CLI 显示 `warn` 的阈值；不参与切换决策 |
| `quota.exhausted_pct` | `100.0` | 0~100 | CLI 显示 `full` 的阈值；不参与切换决策 |
| `quota.fetch_timeout_ms` | `20000` | 毫秒 | 单次 quota 查询 attempt 的超时；需盖住 Codex app-server（≤20s）与 Kimi 401 自愈；超时后按 `quota.fetch_retries` 决定是否重试 |
| `quota.fetch_retries` | `1` | 次 | quota 查询失败后额外重试次数；最多 5 次，401/403/429 不重试 |
| `quota.fetch_retry_delay_ms` | `500` | 毫秒 | 首次 quota 重试等待时间；后续按 500ms、1s、2s、4s、8s 指数退避 |
| `quota.min_refresh_interval_ms` | `90000` | 毫秒 | 同一账号两次真实 quota 查询的最小间隔；缓存比它新就复用、不打端点。daemon 与 CLI 共用 `quota_cache.json`，节流对两条路径同时生效 |
| `quota.failure_backoff_max_ms` | `900000` | 毫秒 | 连续查询失败时的退避上限；退避从 `min_refresh_interval_ms` 起按连续失败次数翻倍并封顶到此值。失败结果不进缓存，没有它时查不出的账号会被每轮重打，反而比健康账号更高频 |
| `token.refresh_slack_ms` | `300000` | 毫秒 | Claude token 距过期此值内 → 触发预刷新 |
| `daemon.poll_interval_ms` | `60000` | 毫秒 | 活跃时轮询间隔 |
| `daemon.idle_threshold_ms` | `1800000` | 毫秒 | provider probe 文件 mtime 距今超过此值视为「用户没在用」 |
| `daemon.idle_poll_interval_ms` | `900000` | 毫秒 | 空闲时轮询间隔 |
| `codex.usage_cache_max_age_ms` | `600000` | 毫秒 | wham/usage 字段漂移时，允许使用本地 last_usage 缓存的最大年龄 |

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
- `quota.min_refresh_interval_ms` / `quota.failure_backoff_max_ms` 调小 → Anthropic usage 端点（实测每账号约每分钟仅放行 1 次）更容易被打爆返回 429，且失败账号会持续重打。除非确认端点限流放宽，否则不要下调。
- `daemon.poll_interval_ms` 设过短（< 30s）→ wham/usage 高频请求可能触风控。
- `daemon.idle_*` 的初衷是「用户真没在用 AI 时别打 quota 请求」，**不要**通过缩小 `idle_threshold_ms` 把空闲化掉。

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

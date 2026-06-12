# subswap 运行时配置（config.toml）

`subswap` 与 `subswapd` 的所有数值调优参数都从一个文件加载：

```
Linux:   ~/.config/subswap/config.toml
macOS:   ~/Library/Application Support/dev.subswap.subswap/config.toml
Windows: %APPDATA%\subswap\subswap\config\config.toml
```

文件**可以不存在**：缺则使用编译期默认值（`crates/core/src/defaults.rs`）。
文件**可以只写部分字段**：未写的字段沿用默认。

## 热加载

- `subswapd` 每轮循环开头读一次；改了文件下一轮（≤ 60s）就生效。
- `subswap` CLI 每次启动读一次；下次执行就生效。
- **解析失败不会拖挂 daemon**：保留上一次成功加载的值，日志里打 `reload config failed; keeping previous values`。
- TOML 里**未识别的字段**会让解析失败（`deny_unknown_fields`），方便你早早发现 typo。

## 完整字段表

| 字段 | 默认值 | 单位 | 说明 |
|---|---|---|---|
| `auto_swap.threshold` | `defaults::AUTO_SWAP_THRESHOLD` | 0.0~1.0 | 任一窗口 `used/limit ≥` 此值 → 自动切换 |
| `auto_swap.cooldown_ms` | `300000` | 毫秒 | 切换后该账号冷却期，daemon 内不再选回 |
| `auto_swap.settle_grace_ms` | `60000` | 毫秒 | 账号刚激活后此窗口内不因 quota loading / 查询失败被自动切走，避免顶掉手动选择 |
| `quota.warn_pct` | `90.0` | 0~100 | CLI 显示 `warn` 的阈值；不参与切换决策 |
| `quota.exhausted_pct` | `100.0` | 0~100 | CLI 显示 `full` 的阈值；不参与切换决策 |
| `quota.fetch_timeout_ms` | `3000` | 毫秒 | 单次 quota 查询 attempt 的超时；超时后按 `quota.fetch_retries` 决定是否重试 |
| `quota.fetch_retries` | `5` | 次 | quota 查询失败后额外重试次数；最多 5 次，401/403 不重试 |
| `quota.fetch_retry_delay_ms` | `500` | 毫秒 | 首次 quota 重试等待时间；后续按 500ms、1s、2s、4s、8s 指数退避 |
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

文档化的字段并不代表「越激进越好」：

- `auto_swap.threshold` 设过低（如 0.5）→ 容易飘出 `Degraded`（两个号都过阈值，policy 不再切，需要手动 swap）。
- `quota.fetch_retries` 最大按 5 次生效；401/403 不重试，其余失败按 `quota.fetch_retry_delay_ms` 指数退避。
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

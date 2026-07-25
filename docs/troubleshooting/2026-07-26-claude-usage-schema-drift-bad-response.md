# 2026-07-26 — Claude 额度显示 `bad response`：usage 响应字段类型漂移 + 失败无退避

## 现象

`subswap` 默认入口里 Claude 账号长期显示旧缓存并挂错误：

```
*  1 a@example.com  7d [ 56% left reset in 5d ]   (cached ~6h ago · bad response)
   2 b@example.com  7d [ 56% left reset in 5d ]   (cached ~4m ago · 429 rate limited)
```

同时账号频繁自动横跳（`flap threshold hit` 反复出现），偶发要求在 Claude Code 里重新登录。

## 一句话结论

**三件事叠在一起，前两件是 subswap 的 bug：**

1. **根因**：Anthropic usage 响应里 `extra_usage.used_credits` 从整数变成小数（`0.0`），
   而 subswap 声明的是 `Option<u64>` → 整份响应 parse 失败 → 该账号额度**永远**查不出，
   CLI 压成 `bad response`。
2. **放大器**：查询失败**不写缓存**，因此没有任何节流，daemon 每轮（60s）都把这个必然失败的
   账号重查一遍（含重试即 2 次请求）。频率反而高于健康账号，把 usage 端点的限流桶打空，
   429 蔓延到同账户下的其他账号 → 出现「另一个号 429」。失败账号常年 `Failed`
   还会让自动切换判定来回翻转，表现为横跳。
3. **上游机制变化（非 bug，但要适配）**：Claude Code 从 **2026-07-09** 起在
   `.credentials.json` 写 `refreshTokenExpiresAt`（实测约 30 天）。refresh token 现在会
   **自然过期**，长期停泊不用的账号到期后必须重新登录，subswap 再刷只会拿到 `invalid_grant`。

## 判别手法

- 用户可见的 `bad response` 由 `render.rs::compact_error` 命中 `"parse"` 得来，对应
  `oauth.rs` 的 `parse usage response: ...`。**只有 HTTP 200 才会走到 parse**，
  所以看到 `bad response` 就能排除鉴权/限流，直接怀疑响应结构。
- 定位实际错误：`subswap --log debug`，看 `quota_query` 的 `quota fetch failed; retrying`
  一行里的 `err=`。daemon 日志**不记** usage 查询失败（只记 refresh 失败），别在
  `subswapd.log` 里找。
- 确认响应结构只准打**一次** curl（端点约每账号每分钟放行 1 次，连发会打爆桶、污染判断）。
- 判断上游凭证结构何时变的：`state/snapshots/` 下按时间排列的 pre-swap 快照就是天然的存档，
  扫一遍字段出现的最早日期即可，不用猜。

## 2026-07 实测响应（节选）

```json
{
  "five_hour": {"utilization": 7.0, "resets_at": "2026-07-25T22:50:00.396399+00:00",
                "limit_dollars": null, "used_dollars": null, "remaining_dollars": null},
  "seven_day": {"utilization": 12.0, "resets_at": "..."},
  "seven_day_oauth_apps": null, "seven_day_opus": null, "seven_day_sonnet": null,
  "seven_day_cowork": null, "seven_day_omelette": null,
  "tangelo": null, "iguana_necktie": null, "nimbus_quill": null,
  "extra_usage": {"is_enabled": true, "monthly_limit": null, "used_credits": 0.0,
                  "utilization": null, "currency": "USD", "decimal_places": 2, ...},
  "limits": [...], "spend": {...}, "member_dashboard_available": false
}
```

要点：`extra_usage` **不再有 `resets_at`**；金额类字段带 `currency` / `decimal_places`，
已是小数语义；新增大量代号窗口（`tangelo`、`omelette` 等）且全为 null。
未知字段本来就被忽略，**真正致命的只有已知字段的类型变化**。

## 修复

| 根因 | 修法 | 落点 |
|---|---|---|
| 字段类型漂移 | 逐字段宽容反序列化：任一字段解不出只退化成 `None`，不再连累整份响应；金额字段改 `f64` | `providers/claude/src/oauth.rs::lenient` |
| 失败无退避 | 失败单独记录并按 `min_refresh × 2^(n-1)`（封顶 `failure_backoff_max_ms`）退避，成功即清零 | `core/src/quota_cache.rs::record_failure` / `in_failure_backoff`，daemon `build_snapshots`、CLI `fetch_quotas_progressive` 两条路径都接 |
| refresh token 到期 | 读 `refreshTokenExpiresAt`，过期就不发刷新请求，直接透出 `needs re-login` | `providers/claude/src/lib.rs::refresh_token_expired` |

## 长期约束

- **usage 响应的任何新字段都必须走宽容解析。** 这是未公开接口，字段类型会在 Claude Code
  版本间漂移；`Option<T>` 只能容忍「字段缺失」，容忍不了「类型变化」，别再靠它兜底。
- **失败结果也要节流。** 只给成功路径加缓存节流是不够的：失败不写缓存 = 失败路径完全没有
  节流，等于给「坏账号」发无限请求配额。新增任何 quota 查询路径都要同时接失败退避。

## 关联

- [2026-06-14 429 vs invalid_grant](2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md)：
  缓存节流与死 token 守卫的由来；本次的失败退避是它只覆盖了成功路径留下的缺口。
- [2026-06-18 live capture 覆盖 refresh token](2026-06-18-live-capture-clobbers-refresh-token.md)：
  `refreshTokenExpiresAt` 到期与「store 里 refresh 被覆写」是两个不同的重登原因，别混。

# 2026-07-26 — Claude 额度显示 `bad response`：usage 字段类型漂移 + 失败无退避

## 现象

```
*  1 a@…  7d [ 56% left …]  (cached ~6h ago · bad response)
   2 b@…  7d [ 56% left …]  (cached ~4m ago · 429 rate limited)
```

账号频繁横跳（`flap threshold hit`），偶发要求 Claude Code 重登。

## 根因（三层）

1. **根因**：Anthropic usage 里 `extra_usage.used_credits` 从整数变小数（`0.0`），声明 `Option<u64>` → 整份 parse 失败 → CLI 压成 `bad response`。
2. **放大器**：失败**不写缓存** → 无节流；daemon 每轮（**60s**）重查（含重试 = 2 次）→ 打空限流桶，429 蔓延；失败账号常年 `Failed` 使自动切换翻转。
3. **上游**：Claude Code 自 **2026-07-09** 写 `refreshTokenExpiresAt`（实测约 **30 天**）。长期停泊到期后刷只会 `invalid_grant`，须重登。

## 判别

- `bad response` ← `render.rs::compact_error` 命中 `"parse"` ← `oauth.rs` `parse usage response`。**仅 HTTP 200 才到 parse** → 排除鉴权/限流，疑响应结构。
- `subswap --log debug` 看 `quota_query` 的 `quota fetch failed; retrying` 里 `err=`。daemon 日志**不记** usage 失败（只记 refresh），别在 `subswapd.log` 找。
- 确认结构只准打**一次** curl（约每账号每分钟 1 次）。
- 凭证结构何时变：扫 `state/snapshots/` pre-swap 快照字段最早日期。

## 2026-07 实测要点

`extra_usage.used_credits: 0.0`（小数）；`extra_usage` **不再有 `resets_at`**；金额带 `currency` / `decimal_places`；新增代号窗口（`tangelo` 等）多为 null。未知字段本被忽略；**致命的是已知字段类型变化**。

## 修复

| 根因 | 修法 | 落点 |
|---|---|---|
| 类型漂移 | 逐字段宽容反序列化，解不出 → `None`；金额改 `f64` | `providers/claude/src/oauth.rs::lenient` |
| 失败无退避 | `min_refresh × 2^(n-1)`（封顶 `failure_backoff_max_ms`），成功清零 | `core/src/quota_cache.rs::record_failure` / `in_failure_backoff`；daemon `build_snapshots`、CLI `fetch_quotas_progressive` |
| refresh 到期 | 读 `refreshTokenExpiresAt`，过期不刷 → `needs re-login` | `providers/claude/src/lib.rs::refresh_token_expired` |

## 长期约束

- usage **任何新字段必须走宽容解析**；`Option<T>` 只容忍缺失，不容忍类型变化。
- **失败也要节流**；新增 quota 查询路径须同时接失败退避。

## 关联

- [2026-06-14](2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md)：成功路径节流留下的失败退避缺口。
- [2026-06-18](2026-06-18-live-capture-clobbers-refresh-token.md)：`refreshTokenExpiresAt` 到期 ≠ store refresh 被覆写。

<!-- 该文档整理/压缩于 2026-09-05 -->

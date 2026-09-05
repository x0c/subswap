# 2026-06-14 — Claude 用量查不出来：429 限流 vs invalid_grant 死 token

## 现象

Claude 账号忽好忽坏、全员 `(cached ~Nm ago)`；Claude Code 能用。codex 两号无限横跳（**无关老 bug**，见 [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) 振荡检测）。

## 根因（两个独立）

1. **usage 429**：全员并发查打爆端点 → 回落旧缓存。
2. **parked `invalid_grant`**：store refresh 已被 Claude Code 轮换作废，daemon 拿死 token 反复刷成风暴。

## 关键教训

- **429 ≠ token 失效**。旧 KB「429 是鉴权伪装」是误判。判别：有效 token **间隔 4 秒**打 usage → `200 → 429 → 429` + `retry-after`。
- **禁止**手动 `curl` usage 连发复现（会打空桶；曾连发 6 次把 retry-after 顶到 **327s**）。约**每账号每分钟 1 次**。
- 三种信号：usage 429 / refresh 400 `invalid_grant` / usage 401（active live 过期，交还 Claude Code）——处理路径不同，见 PROVIDER_KB「Usage 接口异常状态码」。
- 证据：`subswapd.log` 里 `invalid_grant` 远多于 429 时，慢性根因是死 token。

## 修复（不碰「不刷 active」红线）

| 根因 | 修法 | 落点 |
|---|---|---|
| 429 | 缓存 < `min_refresh_interval_ms`(**90s**) 复用 | `quota_cache.rs::fresh`、`cmd/default.rs`、`daemon::build_snapshots` |
| invalid_grant 风暴 | 指纹判死、跳过刷新、`needs re-login` | `ClaudeProvider.dead_refresh`、`render.rs::compact_error` |
| parked 陈旧 | daemon 每轮 live→store 回灌 | `ClaudeProvider::reconcile_active_from_live` |

## 关联

- [2026-06-08](2026-06-08-codex-refresh-token-already-used.md)：「不刷 active」不变量由来。

<!-- 该文档整理/压缩于 2026-09-05 -->

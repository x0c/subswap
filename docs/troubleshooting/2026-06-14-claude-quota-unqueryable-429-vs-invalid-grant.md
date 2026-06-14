# 2026-06-14 — Claude 用量查不出来：429 限流 vs invalid_grant 死 token（两个独立根因）

## 现象

`subswap` 默认入口里 Claude 账号忽好忽坏：经常全员显示 `(cached ~Nm ago)` 旧缓存查不到实时用量，
过一会儿又能查出来；账号本身在 Claude Code 里能正常用。codex 账号同时在两个号之间无限横跳。

## 一句话结论

**两个互相独立的根因叠在一起，别只抓一个：**

1. **usage 端点限流极严（429）**：subswap 把所有账号一起并发查，打爆端点 → 全员 429 → 回落旧缓存。
2. **parked 账号 refresh token 变死（invalid_grant）**：某些号 subswap 存的 refresh token 已被
   Claude Code 轮换作废，daemon 拿死 token 反复刷成风暴，那个号永远查不出。

codex 横跳是**第三件无关的老 bug**（防抖刹车被冷却躲过），见
[AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) 的振荡检测段。

## 关键排查教训（避免重蹈覆辙）

- **429 ≠ token 失效**。旧 `PROVIDER_KNOWLEDGE_BASE.md` 写「429 是鉴权失败的伪装」，是误判，
  害我一开始抓错方向。判别实验：拿**确认有效**的 token（Claude Code 维护的 active 账号），
  **间隔 4 秒**打 usage 端点 → `200 → 429 → 429` + `retry-after`。证明 429 是端点真限流，与 token 无关。
- **别手动 `curl` usage 端点连发几次去"复现"**——会自己把限流桶打空、污染判断（我连发 6 次就把
  retry-after 顶到 327s，误以为是稳定限流）。usage 端点约**每账号每分钟才放 1 次**。
- 三种「查不出」信号要分清：usage 429（限流）/ refresh 400 `invalid_grant`（parked 死 token）/
  usage 401（active live token 过期，交还 Claude Code）。处理路径完全不同，见
  [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的「Usage 接口异常状态码」。
- 证据来源：`subswapd.log` 里 `invalid_grant` 出现 346 次、429 仅 3 次、成功刷新 1 次——
  说明慢性根因是 invalid_grant，不是 429。看日志计数比看单次现象靠谱。

## 修复（都不碰「subswap 不刷 active」红线）

| 根因 | 修法 | 落点 |
|---|---|---|
| 429 限流 | 缓存节流：缓存 < `min_refresh_interval_ms`(90s) 就复用、不打端点；daemon+CLI 共用缓存 | `quota_cache.rs::fresh`、`cmd/default.rs`、`daemon::build_snapshots` |
| invalid_grant 风暴 | 死 token 守卫：指纹判死、跳过刷新、显示 `needs re-login` | `ClaudeProvider.dead_refresh`、`render.rs::compact_error` |
| parked token 变陈旧 | 持续回灌：daemon 每轮把 active live token 抓回 store（只 live→store） | `ClaudeProvider::reconcile_active_from_live` |

机制细节见 [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的
「Usage 接口异常状态码」与「Refresh token 轮换与 capture-on-leave」。

## 关联

- [2026-06-08 refresh token already used](2026-06-08-codex-refresh-token-already-used.md)：
  「subswap 不刷 active」不变量的由来，本次三道补救都建立在它之上。

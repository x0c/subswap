# 2026-08-21 — Cursor 自动切到 1st 0% 的号，旁边还有余量

## 现象

默认入口 Cursor 段出现 `! auto: swapped to <邮箱>`，目标号 `1st` 和 `API` 都是 `0% left`，
同列表里另一个号 `1st` 还有余量（例如 `12% left`），两边 `API` 都是 `0%`。典型输出：

```text
cursor
  ! auto: swapped to calebreyes710513@hotmail.com
  *  5 calebreyes710513@hotmail.com      1st [  0% left reset in 19d]  API [  0% left reset in 19d]
     6 kimberlymercado95579@hotmail.com  1st [ 12% left reset in 25d]  API [  0% left reset in 25d]
```

## 一句话结论

不是额度查串了。Cursor 的 `1st` 和 `API` 是并行产品配额，旧策略把「任一窗口 Exhausted」当成整号
不能用；两边 API 都是 0% 时两个号一起掉进重置兜底，再按 billing cycle 谁先结束选目标，就会切到
`1st` 也是 0%、但重置更早的那个号。

## 根因

`usage-summary` 里 `autoPercentUsed` → `1st`（IDE Auto/Composer），`apiPercentUsed` → `API`，
共用 `billingCycleEnd`。IDE 流量只吃 `1st`；API 耗尽不挡 Auto/Composer。

旧 `decide()`：

1. `account_needs_swap` / `is_viable_candidate` 对**所有**窗口做 Exhausted 硬阻断。
2. 两边 API 都是 0% → 有余量的 `1st` 号也不算候选。
3. 重置兜底取阻塞窗口 `reset_at` 的最大值，选最早恢复的号。Cursor 两窗口 reset 相同，于是变成
   「谁的账期先结束」：19d 的全空号压过 25d、`1st` 还有 12% 的号。

`QuotaStatus::Warn` 只该影响展示。若候选判定还要求必须存在 `Ok` 窗口，`1st` 过了 `warn_pct`
（默认 90% 已用 / 10% 剩余）但未耗尽时，同样会从候选里消失，只剩重置兜底。

## 禁止的误修路径

- 不要取消 Claude 5h/7d、Codex 月度的 Exhausted 硬阻断：那些是同一产品的叠加上限，任一耗尽整号就不能用。
- 不要让 `quota.warn_pct` 重新参与候选排除。
- 不要为了「消掉切到 0% 号」去改手动 `subswap swap`，或用高频 quota 请求去复现。

## 当前状态

**1.6.2 起**：`1st` 还有余量时，不再因 `API` 耗尽把整号判死（修复本文原始症状）。

**【裁定更新 · 2026-09-05】**：Cursor 改为 `1st` / Credits / `API` **三池并行**——任一有余量即可用。
因此「全员 1st 见底、某号 API 仍有余量」必须切到该号，不能只按重置时间挑全空号。
见 [2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md](2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md)。

若列表里仍出现「切到 / 停在 1st 0% 的号、旁边 1st 还有余量」，先看版本是否含 1.6.2+ 修复；
若「全员 1st 0%、旁边 API 还有余量却切到全空号」，需含本次 2026-09-05 三池并行修复的版本。

## 关联

- [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) §1.1 / §2
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的「Cursor」
- `crates/core/src/auto_policy.rs`（`auto_swap_quotas` / `is_viable_candidate`）

# 2026-08-21 — Cursor 自动切到 1st 0% 的号，旁边还有余量

## 现象

```text
cursor
  ! auto: swapped to calebreyes710513@hotmail.com
  *  5 calebreyes…  1st [  0% left reset in 19d]  API [  0% left reset in 19d]
     6 kimberly…    1st [ 12% left reset in 25d]  API [  0% left reset in 25d]
```

## 根因

`usage-summary`：`autoPercentUsed` → `1st`，`apiPercentUsed` → `API`，共用 `billingCycleEnd`。IDE 只吃 `1st`；API 耗尽不挡 Auto/Composer。

旧 `decide()`：`account_needs_swap` / `is_viable_candidate` 对**所有**窗口做 Exhausted 硬阻断 → 两边 API 都是 0% 时有余量的 `1st` 号也不算候选 → 重置兜底取阻塞窗口 `reset_at` 最大值，选最早恢复；Cursor 两窗口 reset 相同，变成「谁的账期先结束」（19d 全空压过 25d/`1st` 12%）。

`QuotaStatus::Warn` 只该影响展示。若候选还要求必须有 `Ok` 窗口，`1st` 过了 `warn_pct`（默认 90% 已用 / 10% 剩余）但未耗尽时同样掉出候选。

## 禁止的误修

- 不要取消 Claude 5h/7d、Codex 月度的 Exhausted 硬阻断（同一产品叠加上限，任一耗尽整号不可用）。
- 不要让 `quota.warn_pct` 重新参与候选排除。
- 不要为「消掉切到 0% 号」去改手动 `subswap swap`，或用高频 quota 请求复现。

## 当前状态

**1.6.2 起**：`1st` 还有余量时不再因 `API` 耗尽判死整号。

**【裁定 · 2026-09-05】**：`1st` / Credits / `API` **三池并行**——任一有余量即可用。见 [2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md](2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md)。

「切到 1st 0%、旁边 1st 还有余量」→ 查是否 ≥1.6.2；「全员 1st 0%、旁边 API 有余量却切全空」→ 需含 2026-09-05 三池修复。

## 关联

- [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) §1.1 / §2
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Cursor」
- `crates/core/src/auto_policy.rs`（`auto_swap_quotas` / `is_viable_candidate`）

<!-- 该文档整理/压缩于 2026-09-05 -->

# 2026-09-05 — Cursor Credits 误当成 `$20` API 池 / 显示 `$0.00`

## 症状

- 用户说某号（如 `terryk2816stone@hotmail.com`）Spending 页还有约 **$11 Credits**；
- subswap 曾显示 `$ [$0.00 left]`，且自动换号停在其它 `0%` 号上；
- 用户纠正：**套餐 `$20` 已含额度一直就是 API 额度，与 `API` 列同一池；Credits 是另一回事。**

## 错误路径（已否决）

曾把 `usage-summary` 的 `individualUsage.plan.used/limit`（Pro 常为 `2000` 分）或
`GetCurrentPeriodUsage.includedSpend` 当成 Credits。那是 **API / 已含池**，耗尽时官方也会说
`You've hit your usage limit`，与 Spending 的 Credits 无关。
`breakdown.bonus` / `bonusSpend` 也不是剩余 Credits。

## 正确来源

网页 Spending 抓包：

`POST https://cursor.com/api/dashboard/get-credit-grants-balance`，body `{}`，
cookie 与 usage-summary 相同（WorkOS session）。

```json
{
  "hasCreditGrants": true,
  "creditBalanceCents": "1110",
  "totalCents": "2500",
  "usedCents": "1390"
}
```

`creditBalanceCents` 为剩余分（`"1110"` → `$11.10`）。无赠送时可能返回 `{}`。

## 自动换号

`1st` 与 Credits 为**并行池**，且 **API 同样参与**（三池任一有余量即可用）。
不能因 `1st` 耗尽就整号不可用（Credits / API 仍可能有余量）。
仅当所有并行池都耗尽才触发/阻断。

详见：[2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md](2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md)。

## 相关

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) Cursor「额度与刷新边界」
- [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) §1.1

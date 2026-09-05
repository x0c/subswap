# 2026-09-05 — Cursor Credits 误当成 `$20` API 池 / 显示 `$0.00`

## 症状

- Spending 页约 **$11 Credits**，subswap 曾显示 `$ [$0.00 left]`，自动换号停在其它 `0%` 号；
- **裁定**：套餐 `$20` 已含额度 = `API` 列同一池；Credits 是另一回事。

## 错误路径（已否决）

曾把 `usage-summary` 的 `individualUsage.plan.used/limit`（Pro 常为 `2000` 分）或 `GetCurrentPeriodUsage.includedSpend` 当成 Credits——那是 **API / 已含池**。`breakdown.bonus` / `bonusSpend` 也不是剩余 Credits。

## 正确来源

`POST https://cursor.com/api/dashboard/get-credit-grants-balance`，body `{}`，cookie 与 usage-summary 相同（WorkOS session）。

```json
{"hasCreditGrants": true, "creditBalanceCents": "1110", "totalCents": "2500", "usedCents": "1390"}
```

`creditBalanceCents` 为剩余分（`"1110"` → `$11.10`）。无赠送时可能返回 `{}`。

## 缺 Credits 列（不是漏查）

官方对该号返回 `{}`（或 `hasCreditGrants != true`）时，subswap **故意不画** Credits 列 =「没有赠送额度」。例：`hillarderdmanpm@outlook.com` → HTTP 200 + `{}` → 无 `$` 列。若同时 `1st`/`API` 也是 `0%`，三池皆空，自动换号不把它当候选。

**禁止的误修**：不要为「列对齐」给无赠送号硬显示 `$ [$0.00 left]`——会与「有赠送但已用尽」混淆。

「显示了 `$ [$0.00 left]` 但 Spending 还有钱」是另一类（误把 API 池当 Credits），见上文。

## 自动换号

`1st` / Credits / **API** 三池并行，任一有余量即可用；仅全部耗尽才触发/阻断。详见 [2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md](2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md)。

## 相关

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) Cursor「额度与刷新边界」
- [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) §1.1

<!-- 该文档整理/压缩于 2026-09-05 -->

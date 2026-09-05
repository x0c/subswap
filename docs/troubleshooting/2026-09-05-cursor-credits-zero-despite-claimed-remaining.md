# 2026-09-05 — Cursor 显示 `$0.00 left`，用户认为还有 Credits 余量

## 症状

- 默认入口某 Cursor 账号（例：`terryk2816stone@hotmail.com`）显示
  `1st [0% left]` / `API [0% left]` / `$ [$0.00 left]`；
- 用户认为该号在官方界面仍有约 **$11** Credits；
- 同时抱怨自动换号「有问题」——停在另一个同样 `0%` / `$0` 的号上，没有切到该号。

## 结论（已用本机 live 令牌核对）

**subswap 的 `$0.00` 与 Cursor 官方用量接口一致，不是解析把 $11 读成了 0。**

对 `terryk2816stone@hotmail.com` 同一份 access token，2026-09-05 实测：

| 接口 | 关键字段 | 含义 |
|---|---|---|
| `GET https://cursor.com/api/usage-summary` | `individualUsage.plan.used=2000`，`limit=2000`，`remaining=0` | 套餐已含额度（分）用尽 |
| 同上 | `breakdown.bonus=47520`，`total=49520` | **已花掉的** bonus 累计，**不是**剩余 Credits |
| `POST api2…/GetCurrentPeriodUsage`（Bearer） | `includedSpend=2000`，`limit=2000`，`bonusSpend=47520`，`remainingBonus=false` | 官方文案 `You've hit your usage limit` |
| `GetPlanInfo` | `includedAmountCents=2000`（Pro $20） | 已含池上限就是 $20 |

因此 CLI 把 Credits 显示成 `$0.00 left`、并把该号视为 Credits **Exhausted** 参与自动换号，与官方账本一致。

同批其它 Cursor 号（kimberly / kochis）同样是 `remainingBonus=false` + hit limit；hillard 的 `limit=0` 另论。全员 gating 窗口都耗尽时，策略走**最早重置兜底**——kimberly 重置更早（约 9d vs terry 16d），故保持停在 kimberly，**不会**因为「用户以为 terry 还有 $11」而切过去。

## 易误读点（禁止再踩）

1. **`individualUsage.plan.breakdown.bonus` ≠ 剩余 Credits。**  
   与 `GetCurrentPeriodUsage.planUsage.bonusSpend` 同量级，是已消耗的促销/赠送用量累计。
2. **`remainingBonus: false`** 表示当前没有可用的 remaining bonus；不要用 `bonus` / `bonusSpend` 数字去「反推还有多少美元」。
3. 若用户坚持官方 UI 仍显示正余量：先让对方指出**具体页面/截图**（Spending / Usage Summary / IDE 状态栏），再对照上表三个接口；已知 Cursor 仪表盘百分比曾有展示滞后（见论坛 GetCurrentPeriodUsage 相关帖），**以接口 `remaining` / `remainingBonus` / `displayMessage` 为准**。

## 自动换号

Credits 参与 gating（见 [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) §1.1 与 PROVIDER_KB）。  
Credits 与 `1st` 都 Exhausted、且没有可用候选时：选 billing cycle **最早结束**的号等待重置；active 已是最早者则 `NoOp`。这不是「没看 Credits」。

## 相关

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) Cursor「额度与刷新边界」
- [2026-08-21 Cursor 自动切到 1st 0% 号](2026-08-21-cursor-auto-swap-to-zero-over-remaining.md)

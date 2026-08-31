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

**已修复（1.6.2）。** 自动切换只看会挡住被切换产品流量的窗口：Cursor 有 `1st` 时忽略 `API`；响应里没有
`1st` 才回退按 `API` 判定。`Warn` 只作展示，仍算可用候选。

若列表里仍出现「切到 / 停在 1st 0% 的号、旁边 1st 还有余量」，先看 `subswap --version` 是否 ≥ 1.6.2，以及
`~/.local/bin/subswap` 的修改时间是否在这次修复之后。1.6.1 及更早（含 2026-08-17 本机安装）不含此修复；
旧二进制会在每次默认入口把 API 0% 的号都打成不可用，再按账期更早选中全空号。

## 关联

- [AUTO_SWAP_DESIGN.md](../design/AUTO_SWAP_DESIGN.md) §1.1 / §2
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的「Cursor」
- `crates/core/src/auto_policy.rs`（`auto_swap_quotas` / `is_viable_candidate`）

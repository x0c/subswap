# 2026-09-05 — Codex 同一账号出现两个 `7d`，其中一个是 `0% left`

## 现象

默认入口 Codex 段某个号显示三个窗口，且有两个都标成 `7d`，例如：

```text
codex
     2 achesjeremy819@gmail.com          5h [100% left reset in 5h ]   7d [  0% left reset in 7d ]   7d [ 84% left reset in 7d ]
```

旁边正常号通常只有 `5h` + 一个 `7d`。

## 一句话结论

不是主额度真的有两个周窗口。`wham/usage` 里除了账号主 `rate_limit`，还有
`additional_rate_limits`（如 `gpt-reserve`）和可选的 `code_review_rate_limit`；旧解析递归扫
整棵 JSON，把附加限额的 `primary_window` 也收成主列表里的第二个 `7d`。附加周限额耗尽时，
列表会出现假的 `7d [  0% left …]`，自动换号还会把主号误判成周额度 Exhausted。

## 根因

`openai_usage::collect_named_windows` 对任意嵌套对象收集 `primary` / `secondary` /
`primary_window` / `secondary_window`。真实响应形状（2026-09-05 实测）：

- `rate_limit.primary_window`：5h（`limit_window_seconds=18000`）
- `rate_limit.secondary_window`：主 7d
- `additional_rate_limits[].rate_limit.primary_window`：`gpt-reserve` 的 7d（常为 `used_percent=100`）

官方 CLI 正常输出只展示主额度窗口，不把附加模型限额画进主状态行。

## 禁止的误修路径

- 不要按「两个 7d 取较松/较紧的一个」在展示层糊弄合并——根因是收错了树。
- 不要为了消掉第二个 `7d` 去改自动换号阈值或忽略所有 `SevenDay` Exhausted。
- 不要用高频 `curl` 打 `wham/usage` 复现；对照缓存或单次实查即可。

## 当前状态

**已修复（1.6.4）。** 递归收集跳过 `additional_rate_limits` / `code_review_rate_limit` /
`model_usage`。顺带：窗口分钟 `28*1440..=31*1440`（如 43200/43800）映射为月度 `mo`。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的「Codex / ChatGPT · Usage 响应字段」
- `crates/providers/codex/src/openai_usage.rs`（`collect_named_windows`）
- `crates/providers/codex/src/quota.rs`（`quota_window_for_usage_window`）

# 2026-09-05 — Codex 同一账号出现两个 `7d`，其中一个是 `0% left`

## 现象

```text
codex
     2 achesjeremy819@gmail.com  5h [100% left …]  7d [  0% left …]  7d [ 84% left …]
```

正常号通常只有 `5h` + 一个 `7d`。

## 根因

`wham/usage` 除主 `rate_limit` 外还有 `additional_rate_limits`（如 `gpt-reserve`）与可选 `code_review_rate_limit`。旧 `openai_usage::collect_named_windows` 递归扫整棵 JSON，把附加限额的 `primary_window` 也收成第二个 `7d`。附加周限额耗尽时出现假的 `7d [  0% left …]`，自动换号误判主号周额度 Exhausted。

实测形状（2026-09-05）：

- `rate_limit.primary_window`：5h（`limit_window_seconds=18000`）
- `rate_limit.secondary_window`：主 7d
- `additional_rate_limits[].rate_limit.primary_window`：`gpt-reserve` 7d（常 `used_percent=100`）

官方 CLI 只展示主额度窗口。

## 禁止的误修

- 不要按「两个 7d 取较松/较紧」在展示层合并——根因是收错了树。
- 不要为消掉第二个 `7d` 去改自动换号阈值或忽略所有 `SevenDay` Exhausted。
- 不要用高频 `curl` 打 `wham/usage` 复现；对照缓存或单次实查即可。

## 当前状态

**已修复（1.6.4）。** 递归收集跳过 `additional_rate_limits` / `code_review_rate_limit` / `model_usage`。窗口分钟 `28*1440..=31*1440`（如 43200/43800）映射为月度 `mo`。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Codex / ChatGPT · Usage 响应字段」
- `crates/providers/codex/src/openai_usage.rs`（`collect_named_windows`）
- `crates/providers/codex/src/quota.rs`（`quota_window_for_usage_window`）

<!-- 该文档整理/压缩于 2026-09-05 -->

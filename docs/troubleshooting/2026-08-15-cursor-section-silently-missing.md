# 2026-08-15 — 整段 Cursor 无声消失（症状族汇总）

## 现象

默认入口**整段 `cursor` 不见**（非单账号余量查不出），零提示。勿直接扎进额度接口。

## 一句话

「整段消失」有三种不同根因、症状相同——都因默认入口静默跳过。**1.5.0 起**：客户端已登录但同步/导入失败时打提示行，例如 `! cursor: signed in as <id> but not tracked (<原因>); run 'subswap login cursor'`。

## 三种已知根因

| 根因 | 关键区分 | 详情 |
|---|---|---|
| 从未导入（仅 CLI、无桌面） | `cursor-agent status` 已登录，账号仓库无 cursor 记录 | [2026-08-14 CLI 已登录但无 Cursor 额度](2026-08-14-cursor-quota-missing-cli-keychain.md) |
| 令牌与身份错配（串号，非消失） | 多行余量/重置时间字节级相同 | [2026-08-14 额度完全相同](2026-08-14-cursor-quota-cloned-across-accounts.md) |
| **删除墓碑拦截自动收入（1.4.17，已移除）** | 曾 `rm`；`~/Library/Application Support/dev.subswap.subswap/removed.json`（Linux `~/.config/subswap/removed.json`）有该 provider/id | 本文 |

## 删除墓碑（1.5.0 已移除）

1.4.17 为防 `rm` 后自动导入立刻加回，引入 `removed.json`；命中时**零提示**静默跳过整段。

**1.5.0 修复**：

- 删除墓碑机制整删（`crates/core/src/removed.rs`、`removed.json`）。客户端仍登录则下次运行自动收入；要彻底消失须在客户端登出。
- `subswap rm` 提示改为：`note: <provider> is still signed in as this account; it will be picked up again on the next run — sign out in the client first to keep it out`（Claude / Codex / Kimi / Cursor；探针用只读 `live_account_id()`）。
- 默认入口新增 `AutoLineKind::Error` 提示行；零账号 provider 也会显示（渲染器旧洞已堵）。

排查：先看是否整段缺失 → 有无 `! <provider>: signed in as ... but not tracked (...)` → 版本是否 ≥ 1.5.0。

## 关联

- [2026-08-14 CLI 已登录但无 Cursor 额度](2026-08-14-cursor-quota-missing-cli-keychain.md)
- [2026-08-14 额度完全相同](2026-08-14-cursor-quota-cloned-across-accounts.md)
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Cursor」

<!-- 该文档整理/压缩于 2026-09-05 -->

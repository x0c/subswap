# 2026-06-08 — Codex/Claude 报 "refresh token already used"，被强制重登

## 现象

> Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.

Claude Code 也可能强制重登（即使用户不手动 swap，只要 subswapd 在跑）。

## 根因

Refresh token **一次性轮换**。旧 subswap 与原生客户端各自独立持有并刷新同一份 → 一方作废。

- **故障 A（陈旧快照覆盖，Codex/Claude）**：store 停在旧 refresh；swap 回写 live → 客户端拿已作废 token 刷新。
- **故障 B（后台抢刷，仅 Claude）**：daemon `keep_claude_tokens_alive` 刷**当前 active**，只写 keyring 不写 `~/.claude` → Claude Code 下次刷新 "already used"。`query_quota` 对 active 的 401 自愈同理。

## 修复（永久不变量）

**不能让 subswap 与原生客户端各自独立轮换 active 账号 token。** Claude active 自 1.16.0 起仅在持有 Claude Code 官方刷新锁时续期（[2026-10-25](2026-10-25-claude-active-quota-expired-while-idle.md)）；Codex/Kimi 只通过官方 app-server / 跨进程锁协调。停泊账号由 subswap 刷新，离开前先把 live 回灌 store。

1. **Capture-on-leave（Codex + Claude）**：`Provider::activate` 覆盖 live 前，读 live → 找 owner → 回写 store。所有 swap 经 `activate`。
   - `crates/providers/codex/src/lib.rs::capture_live_into_store`
   - `crates/providers/claude/src/lib.rs::capture_live_into_store`
2. **不在后台轮换 active（Claude）**：
   - `refresh_if_near_expiry` 开头 `active_account_id()` 命中即跳过；daemon 保活只对 parked。
   - `query_quota` 遇 active 过期 / 401 只在持有 Claude Code 官方刷新锁时续期（`active_refresh.rs`）；识别不到锁协议直接返回错误。

Codex 无后台刷新，只需机制 1。

Shared snapshot-coordination rules now live in the agent CLI integration guide ([Third-party CLI credential coordination](~/.config/agentsync/docs/AGENT_TOOL_CLI_INTEGRATION_GUIDE.md#third-party-cli-credential-coordination-optional-snapshot-coordination-design-only)): native owns active, capture-on-leave with snapshot-first and rollback, terminal-grant-errors stop only that copy. Provider mechanics and recovery steps stay here.

## 用户侧恢复

原生客户端重登一次（`codex login` / Claude Code），subswap 再 import。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Refresh token 轮换与 capture-on-leave」
- [2026-05-29 daemon keyutils](2026-05-29-daemon-keyutils-session-isolation.md)

<!-- 该文档整理/压缩于 2026-09-05 -->

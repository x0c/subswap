# 2026-06-08 — Codex/Claude 报 "refresh token already used"，被强制重登

## 现象

Codex 客户端弹出：

> Your access token could not be refreshed because your refresh token was already
> used. Please log out and sign in again.

Claude Code 也可能出现等价的强制重登（即使用户从不手动 swap，只要 subswapd 在跑）。

## 一句话结论

是 subswap 导致的。根因是 subswap 与原生客户端各自独立持有同一份 **一次性轮换**的
refresh token 并各自刷新，必然有一方被服务端作废。已永久修复。

## 根因

OAuth 的 refresh token 是一次性轮换：刷新一次旧的立即作废。subswap 旧实现存在两条
独立刷新/恢复路径，都会作废原生客户端正在用的 token：

- **故障 A（陈旧快照覆盖，Codex/Claude 都有）**：subswap 把账号凭证当**冻结快照**存。在用
  A 账号期间，原生客户端不断把 live 文件的 token 轮换 R1→R2→R3，而 subswap 副本停在旧值。
  swap 回 A 时把旧 token 写回 live → 客户端拿已作废 token 刷新 → 强制重登。
- **故障 B（后台抢刷，仅 Claude）**：daemon `keep_claude_tokens_alive` 会刷**当前激活、
  Claude Code 正在用的**账号，只写 keyring 不写 `~/.claude`，在服务端把 live token 轮换掉
  → Claude Code 下次刷新即 "already used"。`query_quota` 的 401 自愈对 active 账号同理。

## 修复（永久）

当时确立的不变量是：**不能让 subswap 与原生客户端各自独立轮换 active 账号 token**。Claude active
仍只读不刷；当前 Codex/Kimi 的后续增强只通过官方 app-server / 官方跨进程锁协调，不改变这条根因约束。
停泊（parked）账号由 subswap 刷新/恢复，并在「离开某账号前」先把 live 凭证回灌进账号 store。

1. **Capture-on-leave（Codex + Claude）**：`Provider::activate` 覆盖 live 文件前，读当前
   live 凭证 → 找受管 owner 账号 → 回写其 store。所有 swap（手动 + 自动）唯一经过 `activate`，
   一处生效覆盖两条路径。best-effort，找不到 owner 跳过。
   - 实现：`crates/providers/codex/src/lib.rs::capture_live_into_store`、
     `crates/providers/claude/src/lib.rs::capture_live_into_store`。
2. **绝不轮换 active 账号 token（Claude）**：
   - `refresh_if_near_expiry` 开头加 active 守卫（`active_account_id()` 命中即跳过），
     daemon 保活只对 parked 账号生效。
   - `query_quota` 401 自愈仅当凭证来自 store（parked）时刷新；来自 live（active）时直接返回
     错误，交还 Claude Code 自刷。

Codex 无后台刷新，只需机制 1。

## 用户侧恢复

已踩到该错误的账号需在原生客户端重新登录一次（`codex login` / Claude Code 登录），让
subswap 重新 import 最新 auth.json，后续不再复发。

## 关联

- 设计与机制详见 [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的
  「Refresh token 轮换与 capture-on-leave」。
- 与 [2026-05-29 daemon keyutils session 隔离](2026-05-29-daemon-keyutils-session-isolation.md)
  同属 daemon 保活路径的坑。

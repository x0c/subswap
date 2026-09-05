# 2026-06-18 — capture_live_into_store 用缺 refresh 的 live 快照覆盖 store

## 现象

`swap` 后 Claude Code 强制重登；日志 `token expired/expiring but refreshToken is empty in store; skipping pre-refresh`。重登可恢复。

## 根因

`capture_live_into_store`（Claude / Codex 各一份）曾**无条件**用 live 整段覆盖 store。原生客户端轮换期间 live 可能短暂「有 access、缺 refresh」→ 把 store 可续期的 refresh **永久抹空**。丢失的 refresh 无法凭空找回，须重登。

## 排查（确认是否本 bug）

读 FileStore：`~/Library/Application Support/dev.subswap.subswap/credentials.json`

- Claude：键 `claude:<email>:credentials_json` → `claudeAiOauth.accessToken` / `refreshToken` / `expiresAt`
- Codex：键 `codex:<id>:auth_json` → `tokens.access_token` / `tokens.refresh_token`

`refresh` 空且 `access` 已过期 → 本 bug（代码只能防再发，不能找回）。`refresh` 非空但仍 401/429 → **非本 bug**，见 [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)。

## 暴露面

- **Claude**：`activate` 离开时 + daemon 每轮 `reconcile_active_from_live`（高频，真实复现路径）。
- **Codex**：仅 `activate` 离开时；本次为预防性加固。

## 修复

live 缺 refresh 而 store 有非空 refresh → 跳过用更差快照覆盖。

| Provider | 处理 | 落点 |
|---|---|---|
| Claude | 合并：保留 store refresh，跟进 live access / expiresAt | `crates/providers/claude/src/lib.rs::capture_live_into_store` |
| Codex | 整段跳过回灌（opaque blob，不做字段合并） | `crates/providers/codex/src/lib.rs::capture_live_into_store`，`extract_refresh_token()` |

机制见 PROVIDER_KB「Refresh token 轮换与 capture-on-leave」。

## 关联

- [2026-06-08](2026-06-08-codex-refresh-token-already-used.md)：不能抢刷 active。
- [2026-06-14](2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md)：同批 capture 另一故障模式。

<!-- 该文档整理/压缩于 2026-09-05 -->

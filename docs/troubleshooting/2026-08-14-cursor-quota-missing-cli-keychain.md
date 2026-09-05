# 2026-08-14 — 本机 Cursor 命令行已登录，subswap 却没有 Cursor 额度

## 现象

整段 Cursor 不出现；导入报「请先登录」，但 `cursor-agent status` 已登录。常见于只装 CLI、无桌面应用的 Mac。

## 根因

macOS 上 `cursor-agent` 默认把凭证写进钥匙串：

- service `cursor-access-token` / account `cursor-user`
- service `cursor-refresh-token` / account `cursor-user`

邮箱在 `~/.cursor/cli-config.json` 的 `authInfo`（**不含 token**）。

旧探测顺序：桌面 `state.vscdb`（`~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`）→ 写死 `~/.config/cursor/auth.json` → 都没有则报未登录。官方实际：默认只写钥匙串；文件后端落盘是 `~/.cursor/auth.json`（非 Linux/XDG 的 `~/.config/cursor/auth.json`）。默认入口对导入失败静默跳过。

官方可设 `AGENT_CLI_CREDENTIAL_STORE=file` 改文件后端——Cursor 自己的开关，非 subswap。**1.4.13 起** macOS 已对齐 `~/.cursor/auth.json`，无需为查额度改开关。

## 排查

1. 无整段 `cursor` → 先查「根本没导入」，勿先查额度接口。
2. 无桌面数据目录 → 不可能走桌面来源。
3. `cursor-agent status` 已登录但 subswap 报未登录 → 本条。
4. 钥匙串有 `cursor-access-token` / `cursor-refresh-token`（勿用 `-w` 打 secret）。
5. `~/.config/cursor/auth.json` 与 `~/.cursor/auth.json` 均无、钥匙串有条目 → ≥1.4.13 应能导入；仍失败查桌面空库抢先或钥匙串授权被拒。
6. 版本是否 ≥ 1.4.13。

## 当前状态

**已修复（1.4.13）。** macOS 读/写回 CLI 钥匙串；文件后端对齐官方路径。

**禁令**：读写必须 fork `/usr/bin/security`，禁止 `keyring` crate 写官方条目（ACL 会收成仅 subswap → CLI 反复弹授权；见 [2026-06-11](2026-06-11-claude-code-keychain-acl-poisoning.md)）。已有条目只更新内容，禁止删再建。集成测试必须设 `SUBSWAP_CURSOR_KEYCHAIN_PATH` 指一次性 keychain。不要为看见额度去改 CLI 凭证后端或重登。

## 关联

- [2026-08-15 整段 Cursor 无声消失](2026-08-15-cursor-section-silently-missing.md)
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Cursor」
- [CLI.md](../CLI.md)「Cursor」

<!-- 该文档整理/压缩于 2026-09-05 -->

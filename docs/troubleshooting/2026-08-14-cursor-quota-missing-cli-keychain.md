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

## Cursor CLI 刚登录的新账号没有入池、又回到旧账号

### 现象

`agent login` / `cursor-agent login` 明确显示新邮箱登录成功，但随即运行 `subswap` 时列表没有新账号；再执行 `agent status`，当前账号已变成池内某个旧账号。

### 根因

这不是钥匙串读取失败。Cursor CLI 只有一份当前登录凭证，Subswap 的默认入口和常驻 daemon 都会对这份凭证做自动切换；新账号若在自动切换前尚未写进 Subswap 账号池，就可能被旧池账号覆盖。之后默认入口只会同步**当前仍登录**的旧账号，无法从历史登录过程找回刚才的新账号。

### 排查与安全补救

1. 先运行 `agent status`，确认当前账号是否仍是刚登录的新邮箱；再核对列表中是否已有该邮箱。
2. 若 `agent status` 已回到旧账号，**不要立刻运行 `subswap login cursor`**：它只会再次导入当前旧账号，不能恢复刚才的新账号。
3. 先停止常驻自动切换，再重新执行 Cursor CLI 登录；认证成功后立刻运行 `subswap login cursor`。该导入路径会登记当前账号并打印列表，但不会在收尾时触发自动切换。
4. 看到新账号出现在列表后，再按需要恢复 daemon。若账号本身额度已耗尽，恢复自动切换后它可以被正常切走，但仍应保留在账号池中。

不要通过删除钥匙串条目、改用文件凭证后端或反复查询额度来处理此症状；这些做法不能恢复未入池账号，还可能破坏 Cursor 对官方凭证的访问。

## 关联

- [2026-08-15 整段 Cursor 无声消失](2026-08-15-cursor-section-silently-missing.md)
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Cursor」
- [CLI.md](../CLI.md)「Cursor」

<!-- 该文档整理/压缩于 2026-09-05 -->

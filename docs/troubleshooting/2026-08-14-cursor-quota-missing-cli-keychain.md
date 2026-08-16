# 2026-08-14 — 本机 Cursor 命令行已登录，subswap 却没有 Cursor 额度

## 现象

`subswap` 默认入口只有 Claude / Codex 等其它账号，**整段 Cursor 都不出现**；手动导入报「请先登录 Cursor」，
但同一台机器上 `cursor-agent status` 显示已经登录，对话也正常。常见于只装了 Cursor 命令行、没装桌面应用的 Mac。

## 一句话结论

账号没有丢。macOS 上 Cursor 命令行默认把登录凭证写进系统钥匙串；旧版本只认桌面版登录库和 Linux 风格的
命令行登录文件，两样都没有时会静默跳过导入，列表里就没有 Cursor，额度无从查起。
**1.4.13 起已修复**：macOS 会读（并在切换时写回）官方钥匙串，文件后端则认 `~/.cursor/auth.json`。

## 根因

Cursor 命令行（`cursor-agent`）在 macOS 上的默认凭证后端是钥匙串，条目为：

- service `cursor-access-token` / account `cursor-user`
- service `cursor-refresh-token` / account `cursor-user`

邮箱等展示信息仍在 `~/.cursor/cli-config.json` 的 `authInfo`，**不含 token**。

旧版本的 Cursor 探测顺序是：

1. 桌面版 `state.vscdb`（macOS 默认 `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`）
2. 命令行登录文件（代码写死 `~/.config/cursor/auth.json`）
3. 都没有 → 按桌面路径报「未登录」

这与官方命令行当时的行为对不上：

- 默认不写登录文件，只写钥匙串；
- 即使改成文件后端，macOS 落盘路径是 `~/.cursor/auth.json`，不是旧版认的 `~/.config/cursor/auth.json`
  （后者是 Linux / XDG 路径）。

因此「命令行能用」和「subswap 能列出 Cursor / 查额度」可以同时成立。默认入口对导入失败是静默跳过，
看起来就像从未装过 Cursor。

2026-08-14 在一台无桌面应用、仅命令行已登录的 Mac 上核实：钥匙串条目存在且当天有更新，
`~/.config/cursor/` 与 `~/.cursor/auth.json` 均不存在，账号册无 Cursor 记录。

官方也允许把命令行改成文件后端（`AGENT_CLI_CREDENTIAL_STORE=file` 后再登录）；这是 Cursor 自己的开关，
不是 subswap 的配置。1.4.13 起 macOS 文件后端路径已对齐 `~/.cursor/auth.json`，无需为了查额度去改这个开关。

## 排查方法

1. 先看默认入口有没有 **cursor** 这一段。没有整段，优先查「根本没导入」，不要先查额度接口。
2. 确认本机有没有 Cursor 桌面应用数据目录；没有则不可能走桌面版来源。
3. 用 `cursor-agent status`（或等价登录状态）确认命令行是否已登录。已登录但 subswap 报未登录，就是本条。
4. 看钥匙串是否有 `cursor-access-token` / `cursor-refresh-token`（不要用 `-w` 把 secret 打到终端）。
5. 看 `~/.config/cursor/auth.json` 与 `~/.cursor/auth.json` 是否存在。两者都不在、但钥匙串有官方条目时，
   1.4.13 起应能导入；仍导不进再查是否被桌面版空库抢先、或钥匙串授权被拒。
6. 本机版本是否 ≥ 1.4.13。更旧的版本即使命令行已登录也会整段没有 Cursor。

## 当前状态

**已修复（1.4.13）。** macOS 读取并在切换时写回命令行钥匙串；文件后端对齐官方路径。
读写必须 fork `/usr/bin/security`，禁止用 `keyring` crate 写官方条目，否则会把 ACL 改成「仅 subswap」，
命令行下次读取会反复弹授权框——同类事故见
[2026-06-11 Claude Code keychain ACL 中毒](2026-06-11-claude-code-keychain-acl-poisoning.md)。
已有官方条目只更新内容，禁止删掉再建。集成测试必须把 `SUBSWAP_CURSOR_KEYCHAIN_PATH` 指到一次性 keychain，禁止碰真实登录钥匙串。

首次读取本机钥匙串时系统可能弹一次授权；允许之后稳态不应再弹。不要为了让 subswap 看见额度去改命令行的凭证后端或重登。

## 关联

- [2026-08-15 整段 Cursor 无声消失（症状族汇总）](2026-08-15-cursor-section-silently-missing.md)
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的「Cursor」
- [2026-06-11 Claude Code keychain ACL 中毒](2026-06-11-claude-code-keychain-acl-poisoning.md)
- [CLI.md](../CLI.md) 的「Cursor」

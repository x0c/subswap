# 故障排查索引

| 文档 | 何时该读 |
|---|---|
| [2026-09-05 Cursor 全员 1st 见底却切到全空号（放过 API 余量）](2026-09-05-cursor-auto-swap-to-empty-over-api-remaining.md) | Cursor `! auto: swapped to` 目标 `1st`/`API` 都是 `0%`、旁边号 `API` 还有余量；或改 Cursor 三池并行自动换号前必读 |
| [2026-09-05 Cursor Credits 误当成 `$20` API 池](2026-09-05-cursor-credits-zero-despite-claimed-remaining.md) | Cursor `$ [$0.00 left]` 与 Spending 不符、把 Pro `$20`/API 当成 Credits、某号**没有** Credits/`$` 列、或改 `get-credit-grants-balance` / 并行池换号前必读 |
| [2026-09-05 Codex 同一账号两个 `7d`（附加 gpt-reserve）](2026-09-05-codex-duplicate-7d-from-additional-rate-limits.md) | Codex 行两个 `7d`、其一 `0% left`，或主周额度有余量却被当成周耗尽；改 `wham/usage` 窗口递归 / `additional_rate_limits` 前必读 |
| [2026-08-21 Cursor 自动切到 1st 0% 号](2026-08-21-cursor-auto-swap-to-zero-over-remaining.md) | `! auto: swapped to` 目标 `1st`/`API` 都是 `0%`、旁边号 `1st` 还有余量；改自动换号候选或 Cursor `1st`/`API` 策略前必读 |
| [2026-08-15 整段 Cursor 无声消失（症状族汇总）](2026-08-15-cursor-section-silently-missing.md) | 默认入口整段 Cursor（或任何 provider）不见——不是单账号余量查不出；先分辨三种已知根因，勿直接查额度接口 |
| [2026-08-14 Cursor 多个账号额度数字完全一样](2026-08-14-cursor-quota-cloned-across-accounts.md) | 多个 Cursor 账号余量/重置时间一模一样，或切 CLI 账号后其它号额度跟着变；改 CLI 切换、live 回灌、额度归属或钥匙串写入前必读 |
| [2026-08-14 本机 Cursor 命令行已登录但 subswap 没有 Cursor 额度](2026-08-14-cursor-quota-missing-cli-keychain.md) | 整段 Cursor 不出现、导入报「请先登录」，或 `agent login` 成功后新号没入池且 `agent status` 又回到旧号；改 CLI 凭证探测、钥匙串来源、自动切换或 `auth.json` 路径前必读 |
| [2026-07-28 Claude 能正常使用但 subswap 仍显示旧 401 / 空 access 被回灌](2026-07-28-claude-working-but-quota-stale-401.md) | Claude Code 已恢复但 subswap 仍显示旧 `401 auth failed`，或 parked 长期 429 且 access 变空；改鉴权退避、live→store 回灌或空 access 判定前必读 |
| [2026-07-26 Claude 额度 `bad response` / usage 响应字段漂移](2026-07-26-claude-usage-schema-drift-bad-response.md) | Claude 额度长期 `bad response`、连带 429、账号横跳；改 usage 解析、quota 失败退避或 `refreshTokenExpiresAt` 前必读 |
| [2026-07-09 Codex 用量 401 但 CLI 能正常用](2026-07-09-codex-quota-401-despite-working-cli.md) | Codex 能对话但 subswap 显示 `401 auth failed`；改/排查 app-server 额度查询、control socket、并发安全刷新与 429 fallback 边界前必读 |
| [2026-06-18 live capture 覆盖 refresh token](2026-06-18-live-capture-clobbers-refresh-token.md) | 切换后强制重登、日志 `refreshToken is empty in store`；改 capture-on-leave / capture-on-arrival 前必读 |
| [2026-06-14 429 vs invalid_grant](2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md) | Claude 用量忽好忽坏、全员 cached、账号横跳；改缓存节流或死 token 守卫前必读；判别限流 vs token 失效时查此 |
| [2026-06-11 Claude Code keychain ACL 中毒](2026-06-11-claude-code-keychain-acl-poisoning.md) | 切换后反复弹「security wants to access "Claude Code-credentials"」；改 Claude keychain 读写（禁止 keyring crate，只能 fork `/usr/bin/security`）前必读 |
| [2026-06-08 Codex refresh token already used](2026-06-08-codex-refresh-token-already-used.md) | Codex/Claude 报 `refresh token already used` 强制重登；排查与原生客户端同时刷新竞态时查此 |
| [2026-06-06 filestore 凭据后端](2026-06-06-filestore-credential-backend.md) | 跨平台凭据保存异常、FileStore 读写/迁移失败，或 macOS Claude 激活后仍用旧号时阅读 |
| [2026-05-29 macOS Keychain 弹窗](2026-05-29-macos-keychain-prompts.md) | macOS Keychain 反复弹权限框（历史缓解；根治见 2026-06-06 / 2026-06-11） |
| [2026-05-29 daemon keyutils session 隔离](2026-05-29-daemon-keyutils-session-isolation.md) | Linux daemon 报 credential store NoEntry、CLI 却正常；改 daemon 保活或 Linux keyring 后端前阅读 |
| [2026-05-28 TOML null 序列化](2026-05-28-toml-null-serialization.md) | `registry.toml` 写出 null → `unsupported unit type`；给进 `Account.extra` 的 Option 字段加 serde 属性前必读 |
| [2026-05-28 Claude 配置父目录污染](2026-05-28-claude-config-dir-parent-pollution.md) | `CLAUDE_CONFIG_DIR` 自定义时 `.claude.json` 写到上级；改 `global_config_path` 前必读 |

<!-- 该文档整理/压缩于 2026-09-05 -->

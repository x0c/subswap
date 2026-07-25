# 故障排查索引

> 这是本项目全部故障排查记录的权威来源。排查任何故障前先在此查有无同类前例，避免重新 debug 已解决的问题。

| 文档 | 排查场景 |
|---|---|
| [2026-07-26 Claude 额度 `bad response` / usage 响应字段漂移](2026-07-26-claude-usage-schema-drift-bad-response.md) | Claude 账号额度长期查不出并显示 `bad response`、另一个号连带 429、账号频繁横跳；改 usage 响应解析、quota 失败退避或 `refreshTokenExpiresAt` 处理前必读；排查「上游响应结构变了没」时也查此（含 2026-07 实测响应与判别手法） |
| [2026-07-09 Codex 用量 401 但 CLI 能正常用](2026-07-09-codex-quota-401-despite-working-cli.md) | Codex 账号明明在正常对话、`subswap` 却显示 `401 auth failed`，或改/排查官方 app-server 额度查询、control socket、并发时安全刷新与 429 fallback 边界时必读 |
| [2026-06-18 live capture 覆盖 refresh token](2026-06-18-live-capture-clobbers-refresh-token.md) | 切换后 Claude Code 要求重新登录、日志打 `refreshToken is empty in store`；改 capture-on-leave / capture-on-arrival 逻辑前必读；排查「账号明明在、切过去却被踢下线」或 Codex 账号凭证无故丢失 refresh 时查此 |
| [2026-06-14 429 vs invalid_grant](2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md) | Claude 账号用量忽好忽坏、全员 cached、账号反复横跳；改缓存节流或死 token 守卫前必读；排查「到底是限流还是 token 失效」时查此 |
| [2026-06-11 Claude Code keychain ACL 中毒](2026-06-11-claude-code-keychain-acl-poisoning.md) | 切换后反复弹「security wants to access "Claude Code-credentials"」；改 Claude keychain 读写实现（禁止用 keyring crate，只能 fork /usr/bin/security）前必读 |
| [2026-06-08 Codex refresh token already used](2026-06-08-codex-refresh-token-already-used.md) | Codex 账号报 `refresh token already used` 强制重登；排查 subswap 与 Codex CLI 同时刷新竞态时查此 |
| [2026-06-06 filestore 凭据后端](2026-06-06-filestore-credential-backend.md) | 跨平台凭据保存行为异常、filestore backend 读写失败或迁移时阅读 |
| [2026-05-29 macOS Keychain 弹窗](2026-05-29-macos-keychain-prompts.md) | macOS Keychain 反复弹权限框、凭据访问提示或用户体验异常时阅读 |
| [2026-05-29 daemon keyutils session 隔离](2026-05-29-daemon-keyutils-session-isolation.md) | Linux daemon 与 keyutils session 隔离问题、凭据读取失败时阅读 |
| [2026-05-28 TOML null 序列化](2026-05-28-toml-null-serialization.md) | registry.toml 写出 null 导致反序列化报错、配置保存异常时阅读 |
| [2026-05-28 Claude 配置父目录污染](2026-05-28-claude-config-dir-parent-pollution.md) | Claude 配置目录路径误判、父目录被意外创建或配置隔离失效时阅读 |

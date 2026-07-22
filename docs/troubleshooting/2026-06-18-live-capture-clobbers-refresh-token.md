# 2026-06-18 — capture_live_into_store 用缺 refresh 的 live 快照覆盖 store，账号被静默写死

## 现象

某个 Claude 账号在 `subswap` 列表里看起来正常，但 `swap` 切过去、进 Claude Code 后被强制要求
重新登录；切换时日志打 `token expired/expiring but refreshToken is empty in store; skipping
pre-refresh`。账号本身没坏（重新登录立刻恢复正常），Claude Code 的报错也没错（它确实拿到一份
过期又无法续期的死凭证）。

## 一句话结论

`capture_live_into_store`（Claude 和 Codex provider 各一份）之前**无条件**用读到的 live 凭证
整段覆盖 store。原生客户端（Claude Code / Codex CLI）轮换 token 期间，live 源可能短暂处于
「有 access、缺 refresh」的不完整状态；这一刻被回灌捕获到，就会把 store 里原本可续期的
refresh token 永久抹空。即使当前 Codex/Kimi 已能在官方协调机制内恢复 active 401，也无法凭空找回已经丢失的
refresh token；账号仍需重新登录。

## 排查方法（确认是不是这个 bug）

直接读 FileStore 明文凭证文件（macOS 默认路径
`~/Library/Application Support/dev.subswap.subswap/credentials.json`），按 provider 取出每个
账号的 access/refresh 字段比对：

- Claude：键名 `claude:<email>:credentials_json`，值是 JSON 字符串，解出
  `claudeAiOauth.accessToken` / `claudeAiOauth.refreshToken` / `claudeAiOauth.expiresAt`。
- Codex：键名 `codex:<id>:auth_json`，值同构于 `~/.codex/auth.json`，解出
  `tokens.access_token` / `tokens.refresh_token`。

`refresh` 为空且 `access` 已过期 → 命中本 bug，需要重新登录该账号才能恢复（代码修复防止
**再次**发生，不能凭空找回已经丢失的 refresh token）。

`refresh` 非空但 `access` 未过期、查询仍报 401/429 → **不是本 bug**，按
[PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的「Usage 接口异常状态码」/
「Codex 官方额度通道与刷新边界」分别排查（Codex 长期停泊账号 401 仍是兼容查询的已知限制，不是
本次的回灌覆盖问题）。

## 暴露面：Claude 远大于 Codex

- **Claude**：`capture_live_into_store` 经两条路径触发——`activate` 离开账号时（capture-on-leave）
  **以及** daemon **每一轮**巡检（`reconcile_active_from_live`，对当前 active 账号）。后者频率高，
  撞上坏时机的概率大，这是本次真实复现的路径。
- **Codex**：只在 `activate` 离开账号时触发一次，没有 daemon 周期巡检。本次修复时未找到真实复现
  实例，属于预防性加固（同根因，原理一致，迟早可能撞上）。

## 修复

覆盖前先比较：若本次 live 读取缺 refresh token、而 store 里现有副本有非空 refresh token，
跳过用更差的快照覆盖更好的快照。

| Provider | 处理方式 | 落点 |
|---|---|---|
| Claude | 合并：保留 store 旧 refresh，只跟进 live 的 access token / expiresAt | `crates/providers/claude/src/lib.rs::capture_live_into_store` |
| Codex | 整段跳过本次回灌，保留 store 现有快照（遵循「整段 opaque blob」处理原则，不做字段级合并） | `crates/providers/codex/src/lib.rs::capture_live_into_store`，新增 `extract_refresh_token()`（写法对齐既有 `extract_access_token()`） |

机制细节见 [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的
「Refresh token 轮换与 capture-on-leave」。

## 关联

- [2026-06-08 refresh token already used](2026-06-08-codex-refresh-token-already-used.md)：
  「不能脱离原生客户端协调机制抢刷 active」不变量的由来。
- [2026-06-14 429 vs invalid_grant](2026-06-14-claude-quota-unqueryable-429-vs-invalid-grant.md)：
  同一批 capture-on-leave / capture-on-arrival 机制的另一类故障模式。

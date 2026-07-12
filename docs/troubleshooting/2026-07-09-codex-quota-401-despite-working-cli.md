# 2026-07-09 — Codex 账号明明在正常用，subswap 却查用量 401

## 现象

`subswap` 默认入口里某个 **active** 的 Codex 账号，用量一列显示
`(cached ~Xh ago · 401 auth failed)`，只挂着上次成功查询时的旧缓存。但用户当时**正在用同一个
账号直接跑 `codex` CLI**，对话完全正常，没有任何登录失效提示。

## 一句话结论

不是 subswap 的 bug，也不是 [troubleshooting/2026-06-18](2026-06-18-live-capture-clobbers-refresh-token.md)
那种 refresh token 被覆盖写死的情况。根因是：**codex-cli 不保证每次真正对话请求之后都把刷新后的
access_token 落盘写回 `~/.codex/auth.json`**——它可能在内存里刷新/沿用了服务端能接受的 token 完成对话，
但磁盘上这份文件仍是旧的、按 JWT `exp` claim 早已过期的那份。而 subswap 查用量走的是完全独立的一条
HTTP 请求（见下），直接读磁盘上这份"过期"凭证去打 `wham/usage`，天然拿 401——跟 codex-cli 自己的对话
请求是否成功没有必然关系。

## 排查方法（确认是不是这种情况，而不是账号真失效）

1. 解出 `~/.codex/auth.json`（或该账号在 FileStore 里的 `codex:<id>:auth_json` 快照）里
   `tokens.access_token` 这段 JWT 的 `exp` claim（base64url 解 payload 即可），跟当前时间比：
   - `exp` 已过 → 和 subswap 显示的 `cached ~Xh ago` 时长基本吻合，说明确实是这份文件里的
     token 过期了，不是查询逻辑本身出错。
2. 看 `~/.codex/sessions/<year>/<month>/<day>/rollout-*.jsonl` 有没有**最近时间点**的会话文件——
   有，说明 codex-cli 用同一份 `CODEX_HOME` 目录，确实刚跑通过真实对话。
3. 对比这次最近会话的时间戳和 `auth.json` 的 mtime：如果会话时间**晚于** `auth.json` 最后修改时间，
   说明这次对话没有触发 codex-cli 往这份文件里写回新 token——命中本问题。
4. 排除项：如果 FileStore 里这个账号的 `refresh_token` 字段本身是空的，或者切走过账号导致
   `capture_live_into_store` 覆盖，那是 [2026-06-18](2026-06-18-live-capture-clobbers-refresh-token.md)
   那个问题，走那边的排查路径，不是本条。

## 为什么会这样（技术细节）

subswap 查 Codex 用量的实现（`crates/providers/codex/src/openai_usage.rs::fetch_usage_raw`）：

```
GET https://chatgpt.com/backend-api/wham/usage
Authorization: Bearer <access_token>
ChatGPT-Account-Id: <id>
```

这是 subswap 自己发起的独立请求，跟 codex-cli 内部怎么维护它自己的会话 token 完全无关。
`crates/providers/codex/src/lib.rs::query_quota` → `raw_auth_for_account` → `read_active_auth_if_matches`
拿到的就是 `~/.codex/auth.json` 磁盘上当前那份原文，subswap **不会**、也没法自己刷新它
（Codex OAuth client_id 不公开，见下方「Codex Token 刷新（subswap 不做）」章节）。

本次实测确认：codex-cli 0.142.3 在磁盘上这份 token 的 JWT `exp` 已过期约 33 小时之后，
仍然用同一个 `CODEX_HOME` 正常完成了对话，且完成后 `auth.json` 的 mtime 没有变化。说明 ChatGPT
后端对「聊天/对话」接口的 token 校验，和对 `wham/usage` 这类用量查询接口的校验，**不是同一套
严格程度**——或者 codex-cli 本身做了内存态的刷新但没有持久化。两种可能哪个是真相无法从 subswap
这一侧确认，但结论一致：**只要 codex-cli 这次没有把新 token 写回磁盘，subswap 的用量查询就会拿到
一份"看起来过期"的凭证，查 401 是预期内的行为，不代表账号真的失效。**

## 结论 / 处理

- 无需改代码。等 codex-cli 下一次真正触发落盘刷新（或用户手动 `codex login` 重新登录一次），
  `auth.json` 更新后 subswap 下次查询就会恢复正常。
- 排查时不要一看到 `401 auth failed` 就当账号失效处理——先按上面的步骤确认是不是这种
  「客户端能用、但没落盘」的情况，避免误导用户去重新登录一个其实还活着的账号。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的「Codex Token 刷新（subswap 不做）」：
  subswap 依赖 codex-cli 自己刷新并落盘这一假设的由来；本条是这个假设不总是成立的一个实测案例。
- [2026-06-18 live capture 覆盖 refresh token](2026-06-18-live-capture-clobbers-refresh-token.md)：
  同样表现为 Codex 账号查询异常，但根因是 subswap 自己把 refresh token 覆盖写死，跟本条要先
  分清楚。

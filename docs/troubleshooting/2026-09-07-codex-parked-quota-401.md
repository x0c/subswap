# 2026-09-07 — Codex 只有当前号有额度，停用号全是 `401 auth failed`

## 现象

默认入口 Codex 段：带 `*` 的当前号正常显示 `5h`/`7d`，其余停用号长期 `quota 401 auth failed`。切到那个号、用一阵再切走，有时会暂时恢复，过几小时又坏。

与 [2026-07-09](2026-07-09-codex-quota-401-despite-working-cli.md)（**当前号** 401 但 CLI 能聊）不同：这里是 **停用号** 查额度失败，当前号往往正常。

## 根因（分层）

### A. 停用号 access 过期却从不刷新（初因）

1. OpenAI access token 会过期；停用号完整 `auth.json` 在仓库里（含 refresh），但 access 不再更新。
2. 早期 `CodexRuntime::refresh` 对停用号返回 `Unsupported`，随后只用过期 access 打 `wham/usage` → 稳定 401。

### B. 临时 app-server 自愈仍不够（2026-09-10）

`1.7.3` 曾改为：完整 blob → 临时 `CODEX_HOME` → 官方 app-server → 吸收轮换。实锤仍可出现：

- 日志 `refresh token dead; needs re-login`
- UI 继续 `quota 401 auth failed`
- 对仓库里仍在的 refresh 直连 OAuth 返回 `refresh_token_reused`

即：**refresh 已被消费，新令牌没写回仓库**。模糊把 app-server 失败标成 `DeadToken` 会掩盖这条路径。  
**禁止**把本现象先定性成「本机不是最新版」——`1.7.4` 不改查额度逻辑。

## 社区同类工具怎么做

对照：`xjoker/codex-switch`、`shuv1337/shuvquota`、`ndycode/codex-multi-auth`、`wweggplant/codex-switch` 等。

| 步骤 | 做法 |
|---|---|
| 1 | 直连 `POST https://auth.openai.com/oauth/token` |
| 2 | Codex 公开 `client_id` = `app_EMoamEEZ73f0CkXaXp7hrann` |
| 3 | 成功后立刻写回该号档案（access/refresh/id） |
| 4 | `refresh_token_reused` / `invalid_grant` 等终态禁止再打同一 refresh |
| 5 | 切号前 live→档案同步，避免分叉 |

额度展示仍打 `wham/usage`。

## 现行根治（`1.7.5+`）

1. 停用号：`oauth::refresh_parked_blob` 直连 OAuth → 合并完整 blob → 引擎写回 → `wham/usage`。
2. 当前号：仍只走官方 app-server 协调（本模块 [`app_server`](../../crates/providers/codex/src/app_server.rs)），不抢刷 live。
3. 终态码才标 `DeadToken`；进程内 refresh SHA-256 守卫禁止风暴重试。
4. **禁止**再用临时 app-server 刷停用号当主路径。

## Agent / 人工探针禁令

- 探针若可能消费 refresh：**必须**写回仓库，或保证绝不触发轮换。
- 直连 OAuth 诊断成功 → 立刻写回；见 `refresh_token_reused` → 停止重试，须重登该号。
- **禁止**只根据「已装含某次修复的版本」结案而不看实机日志/上游错误码。

## 临时自救

refresh 已 `reused` / 吊销：对该号重新登录再导入。切号来回救不了已 `reused` 的仓库副本。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Codex 官方额度通道与刷新边界」
- [2026-07-09](2026-07-09-codex-quota-401-despite-working-cli.md)
- [2026-06-08](2026-06-08-codex-refresh-token-already-used.md)

<!-- 2026-09-07 初稿；2026-09-10 社区对照；同日落地直连 OAuth -->

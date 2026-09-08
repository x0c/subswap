# 2026-09-07 — Codex 只有当前号有额度，停用号全是 `401 auth failed`

## 现象

默认入口 Codex 段：带 `*` 的当前号正常显示 `5h`/`7d`，其余停用号长期 `quota 401 auth failed`。切到那个号、用一阵再切走，有时会暂时恢复，过几小时又坏。

与 [2026-07-09](2026-07-09-codex-quota-401-despite-working-cli.md)（**当前号** 401 但 CLI 能聊）不同：这里是 **停用号** 查额度失败，当前号往往正常。

## 根因（已实测）

1. OpenAI access token 会过期；停用号的完整 `auth.json` 仍在凭证仓库里（含 refresh），但仓库里的 access 不再更新。
2. 共享引擎查停用号额度前会调 `runtime.refresh()`；**Codex 实现故意返回 `Unsupported`**，从不刷停用号。
3. 随后只用过期 access 直连 `wham/usage` → 稳定 401。当前号走官方 `codex app-server`，所以看起来「永远只有一个号正常」。

本机复现证据（2026-09-07）：当前号 access 未过期；`caoozc@outlook.com` access 已过期约两天，仓库里仍有 refresh。把该号完整凭证拷进**临时** `CODEX_HOME`，跑官方 `app-server` 的 `account/rateLimits/read`：**不改 live、不写回仓库**即可读到额度，且临时目录里 access 被官方轮换成未过期。说明 refresh 仍活，缺的是 subswap 对停用号走这条官方通道。

## 为什么旧注释说「parked 不能安全物化」

旧路径若只往临时目录塞 **access、清空 refresh**（`SanitizedHome` 模式），官方刷新结果无法安全吸收回仓库 → 会分叉一次性 refresh。  
**完整**从仓库取出停用号 blob（含 refresh）写进独立临时家目录则安全：该 refresh 不在 live `~/.codex`，也不会与正在跑的 Codex 抢刷。

## 可行根治（已落地）

OpenAI **仍无**官方多账号额度 API。社区（`codex-switch` / `codex-quota` / `codex-auth` 等）与本仓现实现一致：

1. 为停用号建临时 `CODEX_HOME`，写入仓库里的**完整** `auth.json`。
2. `CODEX_HOME=<临时> codex app-server --stdio` → `account/rateLimits/read`；认证失败则 `account/read {refreshToken:true}` 再读一次。
3. 读回临时目录里轮换后的 `auth.json`，经已有 `RefreshOutcome::Rotated` 写回仓库（引擎 `refresh_parked_if_needed`；`CodexRuntime::refresh` → `app_server::refresh_parked_blob`）。
4. 再用新 access 走 `wham/usage` 查额度。

access 仍在 `REFRESH_SLACK_MS` 窗口外则不启进程。无 refresh / 二进制不可用 → 降级不拖垮整表；官方认证明确失败 → `DeadToken` / `needs re-login`。

**禁止**：为停用号自实现 OpenAI OAuth；禁止动 live `~/.codex`；禁止对当前号在无官方协调时抢刷；禁止只物化 access、清空 refresh（`SanitizedHome` 仅保护 live 并发）。

## 不能根治的边界

- refresh 已被吊销 / `already used` / 上游要求重登 → 仍须对该号重新登录再导入。
- 本机没有可用的 `codex` 二进制 → 官方通道不可用。
- 停用号很多时，每个号起一次短暂 app-server 会变慢，需节流/缓存（已有 quota 缓存窗口可复用）。

## Agent / 人工探针禁令

排查停用号 401 时，若用临时 `CODEX_HOME` 跑官方 app-server 且触发了 token 轮换：**必须**把临时目录里新的 `auth.json` 写回该账号仓库副本，或保证探针绝不会让官方消费 refresh。  
只读探针若刷了 token 却扔掉临时目录，仓库里的旧 refresh 会立刻作废，表现为 `refresh token dead; needs re-login`，只能重登。

## 临时自救（旧版本 / refresh 已死时）

若仍跑在未含本修复的版本：对该停用号手动切入再切回，或重新登录后导入。  
本修复后若仍 401：多半是 refresh 已被吊销，须对该号重新登录再导入。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Codex 官方额度通道与刷新边界」
- [2026-07-09](2026-07-09-codex-quota-401-despite-working-cli.md)（当前号 / app-server）
- [2026-06-08](2026-06-08-codex-refresh-token-already-used.md)

<!-- 该文档整理于 2026-09-07 -->

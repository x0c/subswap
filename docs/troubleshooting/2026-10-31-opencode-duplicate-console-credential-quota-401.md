# 2026-10-31 — OpenCode 同账号重复 Console 凭证导致 quota 401（号没死，读到了死的那份）

## 现象

- 默认入口里某 OpenCode Console 号显示 `needs re-login`，但该号在官方客户端里能正常聊天。
- 切走后复查：active 号恢复健康，轮到 parked 号报 401；看起来像「谁被查谁死」。
- 用户体感：`swap` 刚切完的那一刻，连当前号也 `needs re-login`，但进 `opencode` 发一句话就好。

## 根因

1. 官方 `opencode.db` 的 `credential` 表允许同一用户+workspace 存多份 `opencode` 凭证（重登会新建 `credential_id`，不替换旧的）。本机实例：`scottethanjfgg@gmail.com` 有两份（`cred_0d8e…` / `cred_17390…`），其中一份 token 已死（status 401），另一份有效（status 200）。
2. subswap 的账号主键是 `console-{accountID}-{orgID}`，两份凭证映射到同一个 registry 账号；`query_console_quota` 按 DB 顺序取第一份就查，读到死 token 就报 `needs re-login`——归属 bug，不是登录真过期。已用两份 token 直调 `GET <server>/console/api/go/status` 验证：一份 401、一份 200。
3. 切号瞬间的当前号 401 是另一回事：切换时旧 access 正处轮换中，subswap 只读不刷所以 401；官方客户端下次使用会自动续上（subswap 绝不能自己拿 refresh 去换，一次性轮换抢跑会把官方的也刷废，直接掉登录）。

## 处理（2026-10-31 落地）

- `query_console_quota`：同账号多份 live 按 registry 记录的 `credential_id` 优先，其余顺试；只在 401/403 时顺延，429/网络/超时直接返回（不借限流时多打）。
- active 号全部 401 时报软状态 `credential expired; open native client`（经 `compact_error` 显示为 `credentials expired; open native client`），不断言重登；parked 号保持 `needs re-login`（没人给它续，续不上）。
- subswap 自己刷 Console refresh 仍是禁止项（见 AGENTS.md 项目不变量）。

## 不采用

- subswap 持 refresh 自助续期：与官方抢一次性 refresh token，最坏把登录刷掉。
- 用 `expires` 字段预判：实测死 token 的 `expires` 仍在未来 20+ 天（服务端撤销/轮换不更新该字段），只能覆盖自然过期，覆盖不了本案。
- `opencode auth list` 触发续期：实测不碰 `time_updated`，无续期作用。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「OpenCode Console official sign-in and Go monitoring」
- [2026-07-09 Codex 用量 401 但 CLI 能正常用](2026-07-09-codex-quota-401-despite-working-cli.md)（同类「额度 401 ≠ 不能用」）

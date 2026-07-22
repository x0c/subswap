# 2026-07-09 — Codex 账号明明在正常用，subswap 却查用量 401

## 现象

`subswap` 默认入口里某个 **active** Codex 账号只显示旧缓存与 `401 auth failed`，但同一账号在 Codex CLI、
VS Code 扩展或桌面端仍能正常对话；退出 Codex 后再查，额度又恢复。

## 一句话结论

账号通常没有失效。旧版 subswap 只读 `~/.codex/auth.json` 里的 access token 直连 `wham/usage`，而官方
Codex 进程可能已在内存中持有更新认证状态，磁盘副本却暂未落盘；退出客户端后落盘，所以查询看似自愈。
现已改为 active 账号优先通过官方 app-server 读取额度，并只在官方安全边界内刷新一次。

## 根因

对话与旧用量查询不是同一条认证路径：

- Codex 原生客户端负责 OAuth 与一次性 refresh token 轮换，运行中的进程可能比磁盘 `auth.json` 更新；
- 旧 subswap 另起 HTTP 请求，把磁盘 access token 发给 `wham/usage`；这枚 token 过期就会 401；
- 强行让 subswap 自己调用 OAuth token 端点，或简单启动/关闭另一个 Codex 进程，都无法证明不会与用户正在运行的
  客户端同时消耗同一枚一次性 refresh token，可能把账号写成必须重登。

因此「客户端能对话」与「旧 subswap 能查额度」并不矛盾；退出后恢复，通常只是官方进程终于把状态同步回磁盘。

## 当前修复

active 账号按以下顺序查询：

1. `<CODEX_HOME>/app-server-control/app-server-control.sock` 存在时，运行
   `codex app-server proxy --sock <socket>`，经官方 JSONL RPC 调 `account/rateLimits/read`，直接复用运行中
   官方进程的最新认证状态。
2. 没有控制 socket、且确认没有普通 Codex 进程时，短暂启动 `codex app-server --stdio`。若额度请求明确
   认证失败，调 `account/read {refreshToken:true}` 让官方客户端强刷一次，再重试一次额度。
3. 没有控制 socket、但普通 Codex 正在运行时，仍用临时 app-server 查询，不过给它一个 `0600` 临时
   `CODEX_HOME`：复制 live `auth.json` 后把 refresh token 清空。这样可以尝试现有 access token，却不可能
   抢刷或覆盖真实凭证。
4. 官方通道不可用、认证失败或方法不兼容时才回退旧 `wham/usage`；官方返回 429 或其他服务错误时直接返回，
   **不再 fallback 发第二条请求**，避免扩大限流。

parked 账号仍走 `wham/usage`。原因不是功能遗漏：共享引擎此处只有 access token，无法把官方刷新后轮换出的
完整凭证安全吸收回账号仓库；临时拼装残缺 `auth.json` 会制造 refresh token 分叉。

## 排查方法

1. 先确认账号是 active 还是 parked。只有 active 会使用官方 app-server；parked 的 401 仍可能要求重登。
2. active 仍异常时检查本机 `codex` 是否足够新、是否支持 `app-server`、`app-server proxy` 和
   `account/rateLimits/read`；不支持时会走兼容回退。
3. 若显示 429，不要重登或连续重试：这是官方额度服务限流，当前实现不会再切换通道放大请求。
4. 若官方刷新也明确拒绝认证，再在 Codex 中重新登录，然后运行 `subswap` 重新导入。
5. 若账号仓库里的 refresh token 本身缺失，或曾被 live 不完整快照覆盖，走
   [2026-06-18 live capture 覆写 refresh token](2026-06-18-live-capture-clobbers-refresh-token.md)，不是本条。

## 不采用的方案

- **subswap 直连 OAuth token 端点**：复制非公开 OAuth 细节，且无法与官方进程共享锁。
- **后台启动 Codex，等刷新后再关掉**：启动不保证触发刷新或落盘；并发时还可能抢刷，且会干扰用户会话。
- **照搬其他账号工具的私有锁**：该锁只约束使用同一工具的进程，官方 Codex 不认识，不能解决跨工具竞争。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Codex 官方额度通道与刷新边界」。
- [2026-06-08 refresh token already used](2026-06-08-codex-refresh-token-already-used.md)：为什么不能带外抢刷。
- [2026-06-18 live capture 覆写 refresh token](2026-06-18-live-capture-clobbers-refresh-token.md)：相似表现、不同根因。

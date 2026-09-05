# 2026-07-09 — Codex 账号明明在正常用，subswap 却查用量 401

## 现象

**active** Codex 显示旧缓存 + `401 auth failed`，但 CLI/扩展/桌面仍能对话；退出 Codex 后再查额度恢复。

## 根因

对话与旧用量查询不是同一认证路径：旧 subswap 读 `~/.codex/auth.json` access 直连 `wham/usage`；官方进程可能已在内存持有更新状态、磁盘未落盘。强行自调 OAuth 或另起 Codex 进程，可能与用户客户端同时消耗一次性 refresh → 强制重登。

## 当前修复（active）

1. `<CODEX_HOME>/app-server-control/app-server-control.sock` 存在 → `codex app-server proxy --sock <socket>`，JSONL RPC `account/rateLimits/read`。
2. 无 socket 且无普通 Codex 进程 → 短暂 `codex app-server --stdio`；认证明确失败时 `account/read {refreshToken:true}` 强刷一次再重试。
3. 无 socket 但普通 Codex 在跑 → 临时 app-server + `0600` 临时 `CODEX_HOME`：复制 live `auth.json` 后**清空 refresh**（可试现有 access，不可能抢刷）。
4. 官方通道不可用时才回退 `wham/usage`；官方 429/服务错误**直接返回，不再 fallback 第二条请求**。

parked 仍走 `wham/usage`：共享引擎只有 access，无法安全吸收官方刷新后的完整凭证回仓库。

## 排查

1. 先分 active / parked（仅 active 走 app-server）。
2. active 异常：`codex` 是否支持 `app-server` / `proxy` / `account/rateLimits/read`。
3. 显示 429 → 勿重登或连续重试。
4. 官方刷新明确拒认证 → Codex 重登后 `subswap` 再导入。
5. refresh 缺失或被不完整 live 覆盖 → [2026-06-18](2026-06-18-live-capture-clobbers-refresh-token.md)。

## 不采用

- subswap 直连 OAuth token 端点（无法共享官方锁）。
- 后台启停 Codex 等刷新（不保证落盘；并发可能抢刷）。
- 照搬其他工具私有锁（官方 Codex 不认识）。

## 关联

- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Codex 官方额度通道与刷新边界」
- [2026-06-08](2026-06-08-codex-refresh-token-already-used.md)
- [2026-06-18](2026-06-18-live-capture-clobbers-refresh-token.md)

<!-- 该文档整理/压缩于 2026-09-05 -->

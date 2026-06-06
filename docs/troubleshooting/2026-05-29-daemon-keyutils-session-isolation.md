# 2026-05-29 · Linux daemon keepalive 空转：keyutils 按 session 隔离

## 现象

Linux 上 `subswapd.log` 每轮都对**所有** Claude 账号（含当前激活账号）报：

```
WARN subswapd: claude token refresh failed
     account=<id> err=credential store: No matching entry found in secure storage
```

但同一时刻 CLI（`subswap` 默认入口）能正常读到这些账号的凭证、quota 显示正常。
表象上「条目存在又不存在」，矛盾。

## 根因

- Linux 的 keyring 后端是编译期默认 feature **`linux-keyutils`**（内核 keyring），
  条目按**内核 session keyring** 隔离（非 secret-service，也不跨重启持久）。
- `subswapd` 由 CLI 经 **`fork + setsid`** 拉起（`crates/cli/src/daemon_spawn.rs`）。
  `setsid` 让 daemon 进入**新的 session**，于是它拿到一个全新的 session keyring，
  里面没有 CLI 在自己 session 写入的条目 → `get_password` 报 NoEntry。
- 结论：**该后端下 daemon 的 Claude token 后台保活（`keep_claude_tokens_alive` →
  `refresh_if_near_expiry`）实际从未成功刷过任何 token**。账号显不显 OK 只取决于
  CLI 自己 session 里那份 token 是否仍未过期。

> macOS（Keychain）/ Windows（Credential Manager）后端是进程间共享 + 持久的，
> 不受此问题影响；daemon 保活在这两端正常。后端对照表见
> [ARCHITECTURE.md §4.1](../design/ARCHITECTURE.md)。

## 连带放大问题

`ClaudeProvider::query_quota` 早期**没有** 401→刷新→重试 兜底：直接拿 keyring 里的
`access_token` 打 usage 接口，过期就原样 401。叠加 daemon 保活空转 →
过期账号在 CLI 永远显示 `quota 401 auth failed`，无法自愈。

## 解决动作

1. **进程内自愈（已做）**：`query_quota` 在 401 且有 `refresh_token` 时，
   best-effort 刷新一次再重试。该路径跑在查询进程（CLI，与 keyring 同 session），
   绕开 daemon 的 session 隔离。保守起见只在 401 时刷、且只重试一次（AGENTS.md #9）。
   切换路径本就有 `best_effort_pre_refresh`，同样在 CLI session，不受影响。
2. **用户侧兜底**：若该账号 `refresh_token` 也已失效（刷新端点同样 401），自愈也救不了，
   需 `subswap login claude` 重新登录。日志里 `log in again if the client returns 401` 即此意。

## 待评估（未做，需决策）

- 让 daemon 与 CLI 共享同一 keyring 可见域（如 keyutils 用 persistent keyring，
  或 daemon 不换 session）——能恢复 daemon 保活，但 keyutils 仍不跨重启持久。
- 或在 Linux 改用 secret-service 后端（跨 session + 持久），代价是需要运行中的
  D-Bus secret service（gnome-keyring 等），无头机可能没有。

二者都是更大的平台层取舍，目前以「进程内自愈」为主，daemon 保活在 Linux 视为 best-effort。

## 相关代码

见 [ARCHITECTURE.md §7 关键代码路径地图](../design/ARCHITECTURE.md)。

# 2026-05-29 · Linux daemon keepalive 空转：keyutils 按 session 隔离

## 现象

Linux `subswapd.log` 每轮对所有 Claude 账号报：

```
WARN subswapd: claude token refresh failed
     account=<id> err=credential store: No matching entry found in secure storage
```

同时 CLI 能正常读凭证、quota 正常。

## 根因

- Linux 默认 feature **`linux-keyutils`**：条目按**内核 session keyring** 隔离，不跨重启。
- `subswapd` 经 `fork + setsid` 拉起（`crates/cli/src/daemon_spawn.rs`）→ **新 session** → 看不到 CLI session 写入的条目。
- 结论：该后端下 daemon `keep_claude_tokens_alive` → `refresh_if_near_expiry` **从未成功刷过**；账号是否 OK 只取决于 CLI session 内 token 是否未过期。

macOS Keychain / Windows Credential Manager 进程间共享+持久，不受影响。后端对照见 [ARCHITECTURE.md §4.1](../design/ARCHITECTURE.md)。

## 连带

早期 `ClaudeProvider::query_quota` 无 401→刷新→重试：access 过期原样 401；叠加 daemon 空转 → CLI 永远 `quota 401 auth failed`。

## 解决

1. **进程内自愈（已做）**：`query_quota` 在 401 且有 `refresh_token` 时 best-effort 刷一次再重试（跑在 CLI 同 session）；只在 401 时刷、只重试一次。切换路径 `best_effort_pre_refresh` 同理。
2. **用户侧**：`refresh_token` 也失效 → `subswap login claude`。日志 `log in again if the client returns 401`。

## 待评估（未做）

- daemon 与 CLI 共享 keyring 可见域（persistent keyring / 不换 session）——仍不跨重启。
- Linux 改 secret-service（跨 session+持久）——需 D-Bus secret service，无头机可能没有。

目前以进程内自愈为主，daemon 保活在 Linux 视为 best-effort。

## 相关代码

[ARCHITECTURE.md §7](../design/ARCHITECTURE.md)

<!-- 该文档整理/压缩于 2026-09-05 -->

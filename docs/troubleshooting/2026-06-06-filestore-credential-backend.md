# 2026-06-06 · 凭证改用明文文件后端，根治 macOS 钥匙串弹窗 + Claude 额度跳过

## 现象

```text
claude
     1 scott…                          # 整行空白
  *  2 strom…  quota skipped on macOS  # 激活 Claude 被跳过
codex
  *  4 strom…  5h […]  7d […]          # 激活 Codex 正常
```

## 根因

[2026-05-29](2026-05-29-macos-keychain-prompts.md)「少碰 Keychain」的副作用：

1. macOS 对 `!active` **主动跳过** quota（`quota_query_would_touch_inactive_keychain`）→ 空白。
2. 激活 Claude：先读 `~/.claude/.credentials.json`；新版 Claude Code 凭证在钥匙串 `Claude Code-credentials`，文件不存在 → 落进「需钥匙串」分支 → 默认禁用 → `quota skipped on macOS`。Codex 有真实 `~/.codex/auth.json`，不受影响。

## 方案：默认 `FileStore`

- `crates/core/src/store.rs::FileStore`：`<data_dir>/credentials.json`，Unix `0600`，fs2 锁 + rename 原子写。
- CLI `AppContext::build()` 与 daemon `run()` 默认 `FileStore::with_legacy_keyring(...)`。
- **懒迁移**：`get` 未命中 → 旧 KeyringStore 读出落盘；首启可能弹一次，之后不碰钥匙串。
- 移除 skip/门控：`quota_query_would_touch_inactive_keychain`、`quota_keychain_access_enabled` / `keychain_write_back_enabled` / `is_active_account`（claude）、`active_keychain_repair_enabled`（codex）；废弃 `SUBSWAP_QUERY_INACTIVE_KEYCHAIN`、`SUBSWAP_SYNC_KEYCHAIN_ON_START`。

## macOS 从未捕获过 Claude 凭证

换 FileStore 后 codex 迁移成功、claude 全 `missing credentials`——仓库本无 `claude:*`。根因：`import_active` 只读不存在的 `~/.claude/.credentials.json`。

**修法**：`read_claude_code_keychain`（`service = "Claude Code-credentials"`，与 `.credentials.json` 同构）。

- `import_active` / `read_live_credentials`：文件缺失回落该 item。
- `load_credentials`：仓库未命中时，对**当前激活**（`~/.claude.json` 的 `oauthAccount.emailAddress`）一次性捕获 → FileStore；之后走文件。
- 非激活仍 `missing credentials`（钥匙串只存当前激活）——须 `swap` 过去或 `login`；macOS 固有限制。
- 捕获副本含 `refreshToken`，过期由自身 401→refresh→写回 FileStore。

## 切换必须同步 Claude Code Keychain

macOS Claude Code 读 Keychain，不认 subswap 写的 `.credentials.json`。只写文件+registry → 状态页显示目标号、Claude Code 仍用旧号并回写 `~/.claude.json`。

因此 `ClaudeProvider::activate` 须先快照并写入 Claude Code Keychain，再更新文件/registry；后续失败须恢复原 Keychain。capture-on-leave **只读 Keychain**；读不到直接跳过，**禁止回落** `.credentials.json`（stale 文件会错误归属并覆盖 FileStore）。

## 影响

- 状态页激活/非激活均出 quota，无 `skipped on macOS`；默认入口不再反复弹钥匙串。
- Linux keyutils session 隔离（[2026-05-29](2026-05-29-daemon-keyutils-session-isolation.md)）随之消失。
- 代价：token 明文 `0600`，与 Codex `auth.json` 安全模型对齐。

## 验证

- `cargo clippy --workspace`；`cargo test --workspace`（含 FileStore roundtrip / 命名空间 / `0600`）。
- 覆盖安装后真实状态页：各账号出 quota、无 `skipped`、无反复弹框。

<!-- 该文档整理/压缩于 2026-09-05 -->

# 2026-05-29 · macOS Keychain 反复弹授权框

> **后续**：本文是「尽量少碰 Keychain」的历史缓解；副作用是 Claude 激活 quota 被误跳过、inactive 空白。根治改为默认 `FileStore`（见 [2026-06-06](2026-06-06-filestore-credential-backend.md)）。下文 `SUBSWAP_QUERY_INACTIVE_KEYCHAIN` / `SUBSWAP_SYNC_KEYCHAIN_ON_START` 与 skip 逻辑**已移除**。Claude Code 官方 item ACL 中毒见 [2026-06-11](2026-06-11-claude-code-keychain-acl-poisoning.md)。

## 现象

```text
subswap wants to use your confidential information stored in "subswap" in your keychain.
```

输入登录密码后仍可能继续弹。

## 根因（叠加）

1. Keychain 授权绑应用身份/item；重编译、覆盖安装、路径变 → 旧授权失效。
2. `service = "subswap"` 多 item；扫全员 quota 多次弹。
3. 默认入口自动拉 daemon → 前台外再读 Keychain。
4. 早期默认同步写回 Keychain + 查 inactive quota。
5. 只改源码未覆盖 PATH 中二进制 → 仍跑旧版（本机曾是 `/Users/geraltgraham/.local/bin/subswap`）。

## 验证（修此类必须验真实二进制）

```bash
type -a subswap
pkill -f 'subswap __daemon' || true
pkill -f 'subswapd' || true
cargo build -p subswap-cli --release
install -m 755 target/release/subswap /Users/geraltgraham/.local/bin/subswap
/Users/geraltgraham/.local/bin/subswap --help
/Users/geraltgraham/.local/bin/subswap
pgrep -af 'subswap|subswapd' || true
```

## 调试注意

- 搜含反引号文本用单引号，否则 shell 当命令执行触发弹窗。
- `quota quota skipped ...` / `credential store: quota skipped on macOS ...` → 有意跳过，渲染层勿当红色错误。
- active `quota keyring error` → registry active 与本地文件不一致，先 metadata-only sync；仍见旧输出查 `type -a subswap`。

## 相关代码

- `crates/cli/src/daemon_spawn.rs`、`cmd/default.rs`
- `crates/providers/codex/src/lib.rs`、`claude/src/lib.rs`

<!-- 该文档整理/压缩于 2026-09-05 -->

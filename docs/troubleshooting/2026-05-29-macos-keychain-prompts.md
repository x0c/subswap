# 2026-05-29 · macOS Keychain 反复弹授权框

> **后续（2026-06-06）**：本文记录的是「尽量少碰 Keychain」的缓解方案，副作用是 Claude 激活账号
> quota 在 macOS 上被误跳过、inactive 账号 quota 空白。最终根治改为**默认换用明文文件凭证后端
> `FileStore`**，彻底不依赖 Keychain（仅首启从旧 Keychain 一次性迁移）。详见
> [2026-06-06-filestore-credential-backend.md](2026-06-06-filestore-credential-backend.md)。
> 下文「修复原则」中 inactive 跳过、`SUBSWAP_QUERY_INACTIVE_KEYCHAIN` / `SUBSWAP_SYNC_KEYCHAIN_ON_START`
> 等环境变量与 skip 逻辑均已随之移除，保留本文仅作历史背景。

## 现象

macOS 上运行 `subswap` 时反复弹系统 Keychain 授权框：

```text
subswap wants to use your confidential information stored in "subswap" in your keychain.
To allow this, enter the "login" keychain password.
```

用户输入登录密码后仍可能继续弹，表现为“无限弹窗”。

## 根因

这是多个问题叠加，不是单一 daemon 问题：

1. macOS Keychain 授权绑定到具体应用身份和具体 keychain item。`subswap` 本地重编译、覆盖安装、
   路径变化或二进制身份变化后，旧授权可能不再适用。
2. `subswap` 以 `service = "subswap"` 存多条凭证，每个账号 / 字段都可能是独立 Keychain item。
   状态页如果扫所有账号 quota，就可能触发多次授权。
3. 早期默认入口会自动拉起后台 daemon。daemon 自己读 Keychain，会在前台命令之外额外触发授权框。
4. 早期默认入口会把本地 active 凭证同步写回 Keychain，并查询 inactive 账号 quota。
   在 macOS 上这会让“只是看状态”的命令也读写 Keychain。
5. 如果只改源码、没有覆盖用户实际 PATH 中的二进制，用户继续运行的还是旧版本。
   本机实际路径曾是 `/Users/geraltgraham/.local/bin/subswap`。

## 验证要求

修这类问题时要验证用户实际运行的二进制，不能只靠单元测试：

```bash
# 确认路径
type -a subswap
# 停旧进程
pkill -f 'subswap __daemon' || true
pkill -f 'subswapd' || true
# 构建并覆盖安装
cargo build -p subswap-cli --release
install -m 755 target/release/subswap /Users/geraltgraham/.local/bin/subswap
# 跑真实二进制
/Users/geraltgraham/.local/bin/subswap --help
/Users/geraltgraham/.local/bin/subswap
# 确认无残留
pgrep -af 'subswap|subswapd' || true
```

## 调试注意事项

- 搜索含反引号的文本用单引号包裹，否则 shell 当命令执行触发 Keychain 弹窗。
- `quota quota skipped ...` → 有意跳过塞进错误通道，渲染层不该当红色错误。
- `credential store: quota skipped on macOS ...` → 渲染层先识别该字符串再归类，否则误报。
- active 账号 `quota keyring error` → registry active 标记与本地文件不一致，先做 metadata-only sync。代码已修用户仍见旧输出则检查 `type -a subswap` 路径是否已被覆盖。

## 相关代码

- `crates/cli/src/daemon_spawn.rs`：daemon 自动拉起控制。
- `crates/cli/src/cmd/default.rs`：默认入口、metadata-only sync、quota 渐进查询。
- `crates/providers/codex/src/lib.rs`：Codex active 文件优先、inactive keychain quota 控制。
- `crates/providers/claude/src/lib.rs`：Claude active 文件优先、keychain 写回控制。

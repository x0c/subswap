# 2026-05-29 · macOS Keychain 反复弹授权框

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

## 修复原则

macOS 上默认 `subswap` 状态页应尽量做到**不碰 Keychain**：

- 不默认自动拉起 `subswapd`；如确需后台保活 / 自动切换，用户显式设置 `SUBSWAP_AUTO_DAEMON=1`。
- 默认入口不把本地 active 凭证写回 Keychain。
- 默认入口只同步非敏感 metadata，用于对齐 active 标记。
- active 账号 quota 优先读本地客户端文件：
  - Codex：`~/.codex/auth.json`
  - Claude：`~/.claude/.credentials.json`
- inactive 账号 quota 默认跳过，不通过 Keychain 查询。
- 如果用户明确要查 inactive 账号 quota，可显式设置 `SUBSWAP_QUERY_INACTIVE_KEYCHAIN=1`。
- 显式操作仍允许使用 Keychain：`subswap login`、`subswap swap`、`subswap rm`。

## 验证要求

修这类问题时，不能只跑单元测试或 debug 产物，必须验证用户实际运行的二进制：

> Agent 收到用户反馈后必须自己执行下面步骤并给出结果；不要只把命令丢给用户。

1. 先确认实际命令路径：

   ```bash
   type -a subswap
   ```

2. 停掉旧后台进程：

   ```bash
   pkill -f 'subswap __daemon' || true
   pkill -f 'subswapd' || true
   ```

3. 构建并覆盖安装到实际路径，例如：

   ```bash
   cargo build -p subswap-cli --release
   install -m 755 target/release/subswap /Users/geraltgraham/.local/bin/subswap
   ```

4. 跑真实安装后的命令，而不是只跑 `cargo test`：

   ```bash
   /Users/geraltgraham/.local/bin/subswap --help
   /Users/geraltgraham/.local/bin/subswap
   ```

5. 确认没有旧进程残留：

   ```bash
   pgrep -af 'subswap|subswapd' || true
   ```

## 调试注意事项

- 搜索包含反引号的文本时必须用单引号或转义反引号。
  例如不要写 `rg "first `subswap`"`，shell 会把反引号里的 `subswap` 当命令执行，
  反而触发 Keychain 弹窗。
- 如果看到 `quota quota skipped ...`，说明把“有意跳过”塞进了 quota 错误通道；
  渲染层又自动加了 `quota` 前缀。跳过 inactive quota 应该显示为空，或显示短提示，不能当红色错误。
- 如果底层错误是 `credential store: quota skipped on macOS ...`，渲染层必须先识别
  `quota skipped on macOS`，再做 `credential store` 归类；否则会把「主动跳过 Keychain」误报成
  `quota keyring error`。
- 如果看到 active 账号显示 `quota keyring error`，通常说明 registry 的 active 标记和本地 active 文件不一致，
  默认入口应先做 metadata-only sync，而不是 fallback 到 Keychain。若代码已修但用户仍看到旧输出，
  优先检查 `type -a subswap` 的首选路径是否已经被 release 产物覆盖，并停掉旧 `subswapd` 后跑真实路径 smoke。

## 相关代码

- `crates/cli/src/daemon_spawn.rs`：daemon 自动拉起控制。
- `crates/cli/src/cmd/default.rs`：默认入口、metadata-only sync、quota 渐进查询。
- `crates/providers/codex/src/lib.rs`：Codex active 文件优先、inactive keychain quota 控制。
- `crates/providers/claude/src/lib.rs`：Claude active 文件优先、keychain 写回控制。

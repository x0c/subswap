# 2026-06-11 · 打开 Claude Code 反复弹「security wants to access "Claude Code-credentials"」

> 这是 macOS Keychain 弹窗系列里**真正的根因**。此前
> [2026-05-29-macos-keychain-prompts.md](2026-05-29-macos-keychain-prompts.md) /
> [2026-06-06-filestore-credential-backend.md](2026-06-06-filestore-credential-backend.md)
> 处理的是 subswap **自己的** `subswap` service item；本文处理的是 subswap 写 **Claude Code 的**
> `Claude Code-credentials` item 时把它的 ACL 弄坏，导致 Claude Code 本体反复弹授权框。

## 现象

macOS 上每次打开 Claude（Claude Code）反复弹系统框：

```text
security wants to access key "Claude Code-credentials" in your keychain.
To allow this, enter the "login" keychain password.
```

特征：**修一阵又复发**（"出了好几次始终修不好"）。

## 根因

macOS Keychain item 的 ACL 只信任「**创建 / 写入它的那个应用**」。

1. Claude Code 自己是 fork `/usr/bin/security` 来读凭证的，所以 `Claude Code-credentials` 正常状态下
   ACL 信任 `security` 本体。
2. subswap 做 Claude 切换时要把目标账号凭证写进同一个 item。旧实现用 Rust `keyring` crate
   （走 security-framework 原生 API）写——这会把 item 的 ACL 重置成「**仅信任 subswap 本体**」。
3. 之后 Claude Code 再用 `security` 读这个 item，读取方（`security`）不在 ACL 里 → 系统每次都弹
   「security wants to access … enter login keychain password」。
4. **为什么会反复 / 间歇**：Claude Code 自己刷新 token 时又用 `security` 把 item 写回，
   ACL 暂时恢复成信任 `security`，弹窗消失；直到**下一次 subswap 切换**用 keyring 再次写坏。
   subswap swap → 弹窗，Claude 刷新 → 自愈，如此往复，所以"怎么修都还会出现"。

同类产品（CCSwitcher、ccswitch、claude-switch、cc-account-switcher 等）**无一例外**都用
`/usr/bin/security` CLI 读写这个 item，正是为了让「创建方」始终是 `security`、与 Claude Code 的
读取方一致。其中 ccswitch 也是 Rust 写的，但**刻意不用 keyring crate**，而是 `Command::new("security")`。

## 修复

`crates/providers/claude/src/lib.rs`：Claude Code keychain 的读 / 写 / 快照 / 回滚四个操作
全部从 `keyring::Entry` 改为 fork `/usr/bin/security`：

- 读 / 快照：`security find-generic-password -s "Claude Code-credentials" -a "$USER" -w`
- 写：先 `add-generic-password -U …` 原地更新；失败（item 不存在 / ACL 已被旧版污染无法更新）
  则 `delete-generic-password` 后再 `add-generic-password`，让 `security` 重新成为创建者、ACL 复位。
- 回滚到「原本无 item」：`delete-generic-password`。

读取也必须一起改：否则 ACL 复位成「仅 security」后，subswap / subswapd 用 keyring 读反而会被挡。

同时移除 `crates/providers/claude/Cargo.toml` 里 macOS 的 `keyring` 依赖（已无引用）。

### 关键不变量

> **永远不要用 `keyring` crate（security-framework 原生 API）写 `Claude Code-credentials`。**
> 必须 fork `/usr/bin/security`，保证 item 创建者与 Claude Code 的读取方（也是 `security`）一致，
> ACL 才不会每次切换都被打坏。

## 验证

```bash
# 切换后，Claude Code 的读取方式（/usr/bin/security）仍能读到 = ACL 健康
~/.local/bin/subswap swap <other>
security find-generic-password -s "Claude Code-credentials" -a "$USER" -w >/dev/null && echo OK
~/.local/bin/subswap swap <back>
security find-generic-password -s "Claude Code-credentials" -a "$USER" -w >/dev/null && echo OK
```

旧版在 `swap` 后这条 `security` 读会失败 / 弹框；新版始终 `OK`。

> 注：GUI 下「打开 Claude 不再弹框」需用户在桌面会话实测确认；命令行只能验证 ACL 健康度。
> 若用户当前 item 已被旧版污染，装上新版后**下一次 swap** 会自动 delete+add 修复（首次可能弹一次）。

## 相关代码

- `crates/providers/claude/src/lib.rs`：`run_security` / `security_find_password` /
  `security_set_password` / `read|snapshot|write|restore_claude_code_keychain`。
- `crates/providers/claude/Cargo.toml`：已删除 macOS `keyring` 依赖。

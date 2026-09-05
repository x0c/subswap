# 2026-06-11 · 打开 Claude Code 反复弹「security wants to access "Claude Code-credentials"」

> 真正根因：subswap 写 **Claude Code** 的 `Claude Code-credentials` 时弄坏 ACL。subswap 自有 `subswap` service 弹窗见 [2026-05-29](2026-05-29-macos-keychain-prompts.md) / [2026-06-06](2026-06-06-filestore-credential-backend.md)。

## 现象

```text
security wants to access key "Claude Code-credentials" in your keychain.
```

特征：**修一阵又复发**。

## 根因

Keychain item ACL 只信任「创建/写入它的那个应用」。

1. Claude Code fork `/usr/bin/security` 读凭证 → 正常 ACL 信任 `security`。
2. 旧 subswap 用 `keyring` crate（security-framework）写 → ACL 重置为「**仅信任 subswap**」。
3. Claude Code 再用 `security` 读 → 不在 ACL → 每次弹框。
4. Claude Code 自刷 token 用 `security` 写回 → ACL 暂时恢复；下次 subswap 切换又写坏 → 间歇复发。

同类产品一律用 `/usr/bin/security` CLI（含 Rust 的 ccswitch，刻意不用 keyring）。

## 修复

`crates/providers/claude/src/lib.rs`：读/写/快照/回滚全部改 fork `/usr/bin/security`：

- 读/快照：`security find-generic-password -s "Claude Code-credentials" -a "$USER" -w`
- 写：先 `add-generic-password -U …`；失败则 `delete-generic-password` 再 `add`（让 `security` 重新成为创建者）
- 回滚无 item：`delete-generic-password`

读取也必须一起改。移除 macOS `keyring` 依赖。

### 关键不变量

> **永远不要用 `keyring` crate 写 `Claude Code-credentials`。** 必须 fork `/usr/bin/security`，保证创建者与 Claude Code 读取方一致。

## 验证

```bash
~/.local/bin/subswap swap <other>
security find-generic-password -s "Claude Code-credentials" -a "$USER" -w >/dev/null && echo OK
~/.local/bin/subswap swap <back>
security find-generic-password -s "Claude Code-credentials" -a "$USER" -w >/dev/null && echo OK
```

旧版 swap 后 `security` 读失败/弹框；新版始终 `OK`。GUI「打开 Claude 不弹框」需桌面会话确认。已污染 item：装新版后**下一次 swap** 会 delete+add 修复（首次可能弹一次）。

## 相关代码

- `run_security` / `security_find_password` / `security_set_password` / `read|snapshot|write|restore_claude_code_keychain`
- `crates/providers/claude/Cargo.toml`：已删 macOS `keyring`

<!-- 该文档整理/压缩于 2026-09-05 -->

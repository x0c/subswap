# 2026-08-15 — 整段 Cursor 无声消失（症状族汇总）

## 现象

默认入口里**整段 `cursor` 都不见了**——不是某个账号的余量查不出来，是连账号带余量一起消失，且
subswap 一句话都不说。用户反馈「Cursor 又不显示余量了，这问题反复修不好」时，先按此文排查，
不要直接扎进额度接口。

## 一句话结论

到目前为止，「整段 Cursor 消失」出现过三种完全不同的根因，但表现出的症状一模一样——都是
默认入口悄悄跳过、零提示。这正是「反复修不好」的真正原因：每次都当成新故障从头查，
其实是同一个「静默失败」的坏习惯在不同代码路径里重复。1.5.0 起把这条堵上：默认入口在
客户端确实登录着、但同步/导入失败时会打一行提示（例如
`! cursor: signed in as <id> but not tracked (<原因>); run 'subswap login cursor'`），
不会再是单纯的空白。

## 三种已知根因，如何一眼分辨

| 根因 | 关键区分特征 | 详情 |
|---|---|---|
| 从未导入过（本机只装了命令行、没装桌面应用） | `cursor-agent status` 已登录，但 subswap 从没出现过 Cursor 段；账号仓库里没有任何 cursor 记录 | [2026-08-14 本机 Cursor 命令行已登录但 subswap 没有 Cursor 额度](2026-08-14-cursor-quota-missing-cli-keychain.md) |
| 令牌与身份错配导致串号（不是消失，是显示成别的号） | 多个 Cursor 行余量、重置时间字节级相同 | [2026-08-14 多个 Cursor 账号显示完全相同的额度](2026-08-14-cursor-quota-cloned-across-accounts.md) |
| **删除墓碑拦截自动收入（1.4.17，已移除）** | `rm` 过这个账号，客户端仍登录着；本机 `~/Library/Application Support/dev.subswap.subswap/removed.json`（Linux `~/.config/subswap/removed.json`）里能查到该 provider/id | 本文 |

## 删除墓碑：2026-08-15 复盘

### 背景

1.4.17 修「多个 Cursor 账号额度一样」时，为了不让用户刚 `rm` 掉的账号被默认入口的自动导入立刻加回来，
引入了一份「删除墓碑」（`removed.json`）：`rm` 时记一笔，默认入口自动导入前先查这份名单，命中就跳过。

### 问题

墓碑生效期间没有任何提示。用户 8-14 删过一个 Cursor 账号后，客户端一直登录着（令牌健康、
邮箱对得上、有效期到 10 月），但默认入口此后每次都因为墓碑命中而静默跳过整段 Cursor，
表现和「从没导入过」「令牌串号」两种根因长得一模一样——用户只能感觉到「又不显示了」，
没有任何信息能定位到是删除记忆在作怪。

### 修复（1.5.0）

- **整个删除墓碑机制已移除**（`crates/core/src/removed.rs`、`removed.json` 及相关调用全部删除）。
  行为改为：只要客户端仍登录着，`rm` 掉的账号下次运行就会像其它账号一样被自动收入，`rm` 不再有
  「记住删除」的效果。要让某个号彻底不再出现，请在客户端本身登出。
- `subswap rm` 的「客户端仍登录着」提示文案同步更新为反映新行为：
  `note: <provider> is still signed in as this account; it will be picked up again on the next run — sign out in the client first to keep it out`。
  该提示对 Claude / Codex / Kimi / Cursor 四个 provider 统一生效，判定改用只读的 `live_account_id()`，
  不再借用会写状态的同步方法当探针。
- **默认入口新增「客户端登录着但同步失败」的提示行**：过去这类失败只有 `tracing::debug!`，
  用户侧零提示；现在会作为一行 `AutoLineKind::Error` 提示随账号列表一起打出来，即便该 provider
  当次一个账号都没有也会显示（渲染器过去会把零账号的 provider 整块跳过，连带提示也打不出来，
  这个洞已经堵上）。

### 排查方法

1. 先看默认入口有没有整段某个 provider 缺失。有，先怀疑本文列的三种根因，不要先查额度接口。
2. 有没有看到一行 `! <provider>: signed in as ... but not tracked (...)` 的提示？1.5.0 起，客户端确实
   登录着但同步失败会打这行，直接告诉你失败原因，不需要再猜。
3. 本机版本是否 ≥ 1.5.0。更旧版本仍可能命中删除墓碑或吞掉同步失败。

## 关联

- [2026-08-14 本机 Cursor 命令行已登录但 subswap 没有 Cursor 额度](2026-08-14-cursor-quota-missing-cli-keychain.md)
- [2026-08-14 多个 Cursor 账号显示完全相同的额度](2026-08-14-cursor-quota-cloned-across-accounts.md)
- [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md) 的「Cursor」

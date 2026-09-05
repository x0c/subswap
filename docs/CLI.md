# CLI 命令面

| 命令 | 说明 |
|---|---|
| `subswap` | 默认入口：扫本地自动 import → 立即显示账号骨架 → quota 渐进刷新 → 单 Provider 就绪即做 AutoSwap 决策 → 最终状态；同时 best-effort 拉起 `subswapd`（用户无感） |
| `subswap add-api` | 交互式登记 Claude Code 兼容 API；DeepSeek / Kimi 预设只需输入名称与隐藏 API Key；保存后不自动激活。成功后回到与默认入口同一张余量表 |
| `subswap login <claude\|codex>` | 调用官方 CLI 登录流程，完成后导入/覆盖当前登录账号并标记为 active。成功后回到与默认入口同一张余量表 |
| `subswap login <kimi\|cursor\|opencode>` | **不驱动登录**：用户先在对应客户端登录（OpenCode 也可把 API key 写在 `--` 后面），本命令只导入当前状态并标记为 active。成功后回到与默认入口同一张余量表 |
| `subswap swap [<id\|N>]` | 手动切换；`<id>` 用 id/label/`<provider>/<id>`，`<N>` 用默认入口列出的全局序号。切成功后回到与默认入口同一张余量表。无参只打印编号清单（不查 quota） |
| `subswap rm <id\|N>` | 删除账号（registry + keyring），引用形式同 `swap`。成功后回到与默认入口同一张余量表。显式删过的号打开列表不会自动加回；没删过的当前登录仍会自动收入。要重新纳入已删的号，用对应 provider 的 `login` |
| `subswap run <provider> <id> [-- args]` | 账号隔离启动：把该账号凭证投影到私有目录，设隔离环境变量后启动原生 CLI（codex/claude/kimi/opencode），**不动全局活账号**；退出时吸收轮换后的凭证。Cursor 不支持此模式 |
| `subswap shell <id>` | 起一个导出好隔离环境变量的子 shell，交互里连跑多条命令；provider 从账号推断；退出时吸收凭证 |
| `subswap env <id>` | 打印 `export` 行供 `eval`。**注意**：eval 模式不持锁、退出后不吸收凭证，仅供临时短用 |
| `subswap doctor` | 环境自检 |

### 账号环境隔离（`run` / `shell` / `env`）

与 `swap`（全局原地切换）并存的另一种用法：在不同终端用不同账号**并行**，互不干扰、不改全局活账号。
机制：Codex 设 `CODEX_HOME`；Kimi 设 `KIMI_CODE_HOME`；OpenCode 设 `XDG_DATA_HOME` 与 `OPENCODE_AUTH_CONTENT`
（后两者都走 `crates/providers/common` 的 `IsolatedProvider` 通用实现，注册在 `AppContext.isolated` 查表里）；Claude 设 `CLAUDE_CONFIG_DIR`
（macOS 另设 `CLAUDE_SECURESTORAGE_CONFIG_DIR` 使钥匙串 item 命名空间隔离，走专用分支，不在该表内）。
完整设计、约束、风险见
[docs/design/ACCOUNT_ISOLATION_DESIGN.md](design/ACCOUNT_ISOLATION_DESIGN.md)。

Cursor 的凭证位于桌面应用 SQLite 状态库，切换还需要在客户端退出后用事务写入、再重启确认，无法安全投影成
独立目录。因此 Cursor 只支持全局 `login` / `swap` / `rm` / 额度查询，不支持 `run`、`shell`、`env`。

Claude 隔离只隔离账号身份，不隔离工作环境：`projects` / `sessions` / `plugins` / `skills` /
`commands` 等非账号内容共享全局 `~/.claude`。因此用 `subswap run claude <账号>` 跑出来的进程
仍应能 `--resume` 其它账号先前留下的 Claude Code 会话；如果 resume 看不到会话，优先检查隔离目录
是否把这些非账号目录误做成了私有副本。

```bash
subswap run codex 6 -- --version        # 用 6 号账号在隔离环境跑 codex
subswap run kimi alice-uid              # 隔离启动 kimi（KIMI_CODE_HOME 指到私有目录）
subswap run opencode go-abcd1234        # 隔离启动 opencode（XDG_DATA_HOME + OPENCODE_AUTH_CONTENT）
subswap run claude alice@x.com          # 隔离启动 claude（按 id 引用）
subswap shell 3                          # 进子 shell，环境已隔离到 3 号账号
eval "$(subswap env codex/bob@x.com)"   # 临时把当前 shell 指向某 codex 账号
```

- **并发与全局切换**：同一账号可同时被多个隔离会话借用；隔离会话运行时，手动 `swap` 和自动切换仍可
  切换该账号。若恰逢 OAuth refresh token 轮换，低概率会使其中一个会话需要重新登录；这是为保证全局
  切换始终可用而接受的风险。daemon 仅会跳过该具体隔离中的 Claude 账号的后台保活，避免后台刷新与其抢刷。
- **全局活账号告警**：对当前全局 active 账号起隔离会话会告警——若同时被非隔离客户端使用，可能作废其 refresh token。

### 写操作后回到状态面

`add-api` / `login` / `swap <目标>` / `rm` 成功后，先打一行结果，再打印与无参 `subswap` 相同的账号余量表（编号、`*`、quota 块一致）。这是 status-after-action：改完立刻看见当前号池，不必再跑一次默认入口。

这张表**不是**再跑一遍默认入口。约束：

- 不 `sync_local_active`：刚 `rm` 掉的号不会被当场导回。
- 不 AutoSwap：刚手动 `swap` 过去的号不会被余量刷新顶走。
- 不拉起 daemon。
- 仍走同一套 quota 缓存与节流；缓存够新时不打端点。

不附加这张表：`run` / `shell` / `env`（输出是隔离环境本身）、`doctor`、无参 `swap`（零网络编号清单）、`--json`、失败或取消。

隐藏的一次性命令：`subswap migrate-local` —— 从旧版本地账号目录把账号搬到 subswap。`--help` 里看不到，只给迁移旧数据的人用一次。

辅助二进制 `subswapd`：由 CLI 在默认入口自动 detach 拉起，负责周期 quota 轮询 / 自动切换 / Claude token 后台保活。daemon 仍是 Unix-only，Windows 只提供前台 CLI；macOS 默认不自动拉起，避免后台进程访问 Keychain 触发额外授权弹窗。如需启用 macOS 自动拉起，导出 `SUBSWAP_AUTO_DAEMON=1`。通过 `<state>/subswapd.pid` 上的文件锁保证单实例。关掉：`pkill subswapd`；不想被自动拉起：导出 `SUBSWAP_NO_DAEMON=1`。

## OpenCode Go

`subswap login opencode` 不驱动官方登录：先在 OpenCode TUI `/connect` 粘贴 Go 订阅 key，或把 key 写在 `--` 后面直接导入。切换时只改 `auth.json` 里的 `opencode-go` 项，同文件其它供应商（如 openai / anthropic）原样保留。5 小时滚动窗口走自动换号阈值；周 / 月窗口只在明确耗尽时触发。隔离运行同时设 `XDG_DATA_HOME` 与 `OPENCODE_AUTH_CONTENT`。

## Cursor

`subswap login cursor` 不打开浏览器或复制 Cursor 的登录流程：先在 Cursor 桌面端或 `cursor-agent` 完成登录，再运行
该命令导入当前账号。之后可用通用编号或 `cursor/<邮箱>` 执行 `swap` / `rm`。
只要客户端仍登录着，打开列表就会像 Claude / Codex / Kimi 一样自动收入——包括 `rm` 删过的号：`rm` 只清掉 subswap 这边的
记录，不会记住「这个号被删过」，客户端不登出，下次运行就会自动收回来。要让某个号彻底不再出现，请先在客户端登出。
只装命令行、没装桌面应用时同样能导入：macOS 读官方钥匙串，Linux 读命令行登录文件。若命令行已登录但列表仍没有
Cursor，见 [troubleshooting/2026-08-14-cursor-quota-missing-cli-keychain.md](troubleshooting/2026-08-14-cursor-quota-missing-cli-keychain.md)。
若多个 Cursor 账号余量数字完全一样，见
[troubleshooting/2026-08-14-cursor-quota-cloned-across-accounts.md](troubleshooting/2026-08-14-cursor-quota-cloned-across-accounts.md)。

Cursor 正在运行时，`swap` 会先请求它正常退出，等待进程完全结束后再切换账号，成功后自动重新打开；任一步失败
都会恢复原账号状态，避免 Cursor 退出时把内存中的旧凭证写回磁盘。默认入口显示：
`1st` / `API`（来自 `usage-summary` 百分比）以及 Spending 页的 **Credits 赠送余额**
（`get-credit-grants-balance`，如 `$ [$11.10 left]`）。**无赠送时不显示 `$` 列**（官方返回 `{}`），
不是漏查；有列且 `$0.00` 才表示「曾有赠送、现已用尽」。**Credits ≠ Pro 的 $20 API 已含池**——后者与
`API` 列是同一池。自动换号把 `1st` / Credits / `API` 当**并行可用池**（任一有余量即可用）。口径见
[PROVIDER_KNOWLEDGE_BASE.md](PROVIDER_KNOWLEDGE_BASE.md)「额度与刷新边界」。

## Claude 自定义 API

日常使用：

```bash
subswap add-api
subswap swap deepseek
subswap swap <原 Claude OAuth 账号>
```

`add-api` 默认打开交互向导：

- DeepSeek 预设自动填充 `https://api.deepseek.com/anthropic`、Opus/Sonnet/Haiku 三档模型与 effort；
  用户只需确认名称并输入隐藏 API Key。
- Kimi 预设自动填充 `https://api.kimi.com/coding`、effort 与 `ANTHROPIC_API_KEY` 认证；向导会让用户分别选择
  Opus、Sonnet、Haiku 三档模型。Opus/Sonnet 可选 `kimi-for-coding`、`k3`、`k3[1m]`，Haiku 可选
  `kimi-for-coding` 或 `kimi-for-coding-highspeed`；三档默认均为全部会员档位可用的 `kimi-for-coding`。
  非交互（`--yes`）也使用该默认值，可用 `--opus-model`、`--sonnet-model`、`--haiku-model` 分别覆盖。
- Custom 模式逐项询问端点、认证方式、Opus/Sonnet/Haiku 三档模型与 effort。

为保持 Claude Code 的运行语义，subswap 自动把 Sonnet 作为默认模型、Haiku 作为子任务模型；这两个内部映射
不再出现在添加向导或参数中。
- 保存后只进入现有 Claude 账号列表，不自动切换；编号、`swap`、`rm` 与 OAuth 账号一致。

脚本可使用非交互参数：

```bash
subswap add-api --preset deepseek --api-key "$DEEPSEEK_API_KEY" --yes
subswap add-api --preset kimi --api-key "$KIMI_API_KEY" --yes
```

自定义 API 账号没有 quota，统一标记为 `manual_only`：不能被自动选中；处于 active 时自动换号完全停用。
删除 active 的自定义 API 会被拒绝，必须先 `subswap swap` 切回 OAuth 或其他账号，避免删除恢复信息后
Claude Code 仍停留在无法识别的 API 状态。

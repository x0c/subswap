# CLI 命令面

| 命令 | 说明 |
|---|---|
| `subswap` | 默认入口：扫本地自动 import → 立即显示账号骨架 → quota 渐进刷新 → 单 Provider 就绪即做 AutoSwap 决策 → 最终状态；同时 best-effort 拉起 `subswapd`（用户无感） |
| `subswap add-api` | 交互式登记 Claude Code 兼容 API；DeepSeek 预设只需输入名称与隐藏 API Key；保存后不自动激活 |
| `subswap login <claude\|codex>` | 调用官方 CLI 登录流程，完成后导入/覆盖当前登录账号并标记为 active |
| `subswap swap [<id\|N>]` | 手动切换；`<id>` 用 id/label/`<provider>/<id>`，`<N>` 用默认入口列出的全局序号。无参打印编号清单 |
| `subswap rm <id\|N>` | 删除账号（registry + keyring），引用形式同 `swap` |
| `subswap run <provider> <id> [-- args]` | 账号隔离启动：把该账号凭证投影到私有目录，设隔离环境变量后启动原生 CLI（codex/claude），**不动全局活账号**；退出时吸收轮换后的凭证 |
| `subswap shell <id>` | 起一个导出好隔离环境变量的子 shell，交互里连跑多条命令；provider 从账号推断；退出时吸收凭证 |
| `subswap env <id>` | 打印 `export` 行供 `eval`。**注意**：eval 模式不持锁、退出后不吸收凭证，仅供临时短用 |
| `subswap doctor` | 环境自检 |

### 账号环境隔离（`run` / `shell` / `env`）

与 `swap`（全局原地切换）并存的另一种用法：在不同终端用不同账号**并行**，互不干扰、不改全局活账号。
机制：Codex 设 `CODEX_HOME`；Claude 设 `CLAUDE_CONFIG_DIR`（macOS 另设 `CLAUDE_SECURESTORAGE_CONFIG_DIR`
使钥匙串 item 命名空间隔离）。完整设计、约束、风险见
[docs/design/ACCOUNT_ISOLATION_DESIGN.md](design/ACCOUNT_ISOLATION_DESIGN.md)。

Claude 隔离只隔离账号身份，不隔离工作环境：`projects` / `sessions` / `plugins` / `skills` /
`commands` 等非账号内容共享全局 `~/.claude`。因此用 `subswap run claude <账号>` 跑出来的进程
仍应能 `--resume` 其它账号先前留下的 Claude Code 会话；如果 resume 看不到会话，优先检查隔离目录
是否把这些非账号目录误做成了私有副本。

```bash
subswap run codex 6 -- --version        # 用 6 号账号在隔离环境跑 codex
subswap run claude alice@x.com          # 隔离启动 claude（按 id 引用）
subswap shell 3                          # 进子 shell，环境已隔离到 3 号账号
eval "$(subswap env codex/bob@x.com)"   # 临时把当前 shell 指向某 codex 账号
```

- **独占锁**：同一账号同时只能被一个隔离会话借走（防 refresh token 轮换冲突）；被借走时手动 `swap`
  会拒绝切入该账号，默认入口 / daemon auto-swap 会跳过同 provider 本轮自动切换，daemon 也会跳过其保活。
- **全局活账号告警**：对当前全局 active 账号起隔离会话会告警——若同时被非隔离客户端使用，可能作废其 refresh token。

隐藏的一次性命令：`subswap migrate-local` —— 从旧版本地账号目录把账号搬到 subswap。`--help` 里看不到，只给迁移旧数据的人用一次。

辅助二进制 `subswapd`：由 CLI 在默认入口自动 detach 拉起，负责周期 quota 轮询 / 自动切换 / Claude token 后台保活。Unix-only，但 macOS 默认不自动拉起，避免后台进程访问 Keychain 触发额外授权弹窗；如需启用 macOS 自动拉起，导出 `SUBSWAP_AUTO_DAEMON=1`。通过 `<state>/subswapd.pid` 上的文件锁保证单实例。关掉：`pkill subswapd`；不想被自动拉起：导出 `SUBSWAP_NO_DAEMON=1`。

## Claude 自定义 API

日常使用：

```bash
subswap add-api
subswap swap deepseek
subswap swap <原 Claude OAuth 账号>
```

`add-api` 默认打开交互向导：

- DeepSeek 预设自动填充 `https://api.deepseek.com/anthropic`、主模型、三档角色模型、subagent 模型与 effort；
  用户只需确认名称并输入隐藏 API Key。
- Custom 模式逐项询问端点、认证方式、模型映射与 effort。
- 保存后只进入现有 Claude 账号列表，不自动切换；编号、`swap`、`rm` 与 OAuth 账号一致。

脚本可使用非交互参数：

```bash
subswap add-api --preset deepseek --api-key "$DEEPSEEK_API_KEY" --yes
```

自定义 API 账号没有 quota，统一标记为 `manual_only`：不能被自动选中；处于 active 时自动换号完全停用。
删除 active 的自定义 API 会被拒绝，必须先 `subswap swap` 切回 OAuth 或其他账号，避免删除恢复信息后
Claude Code 仍停留在无法识别的 API 状态。

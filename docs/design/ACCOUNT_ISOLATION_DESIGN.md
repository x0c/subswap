# 账号环境隔离设计（subswap run / shell）

> 提案状态：**已实现并验证**——Codex 与 Claude 隔离路径、`run`/`shell`/`env` 三个子命令、
> checkout 独占锁、daemon 保活避让均已落地。本文覆盖隔离启动、checkout 锁、daemon 避让、macOS
> 钥匙串命名空间逻辑，并说明它与现有 in-place `swap` 模型如何并存。
>
> 已落地代码：
> - `crates/core/src/checkout.rs`：flock 独占锁 + `is_checked_out` 探测 + 隔离目录。
> - `crates/providers/codex/src/lib.rs`：`export_auth_blob` / `absorb_auth_blob`。
> - `crates/providers/claude/src/lib.rs`：`export_isolated_credentials` / `materialize_isolated` /
>   `absorb_isolated` / `isolated_keychain_service`（macOS 命名空间 service 名，公式见 §2.1）。
> - `crates/cli/src/cmd/run.rs`：`subswap run <provider> <id>` / `shell <id>` / `env <id>`。
> - `crates/daemon/src/unix.rs`：`keep_claude_tokens_alive` 跳过 checked-out 账号。
>
> 验证：全工作区测试通过；Codex `run` 用桩 CLI 端到端跑通（materialize/absorb/锁）；Claude `run`
> 用桩 `claude` 端到端验证账号文件独立、`projects` / `plugins` 共享链接、`settings.json` 剥离受管 API env；
> Claude `env` 在一次性 keychain 上验证命名空间 item 写入与读回、文件权限、oauthAccount。
>
> **实机实测 2026-06-15 结论**：钥匙串植入公式（§2.1）完全正确，claude 2.1.177 可正常读到命名空间 item。
> 真正的阻断点是 `hasCompletedOnboarding` 门禁（见 §2.3）——已在 `materialize_isolated` 中修复。

## 1. 目标与动机

现状 `swap` 是「全局单活账号 + 原地覆盖」：把某账号凭证覆盖写进全局唯一位置
（`~/.claude/.credentials.json`、macOS 钥匙串、`~/.codex/auth.json`），全机同一时刻只有一个活账号。

本提案新增一种消费方式：**每次启动子进程时把指定账号投影到一个私有目录，用环境变量
让该 CLI 只看自己的目录**，从而：

- 在不同终端用不同账号**同时**跑 claude / codex，互不干扰；
- 不动全局活账号，临时借用某号执行一条命令后即归还。

前提已满足：`FileStore` 已存所有账号的完整凭证（`credentials.json` / `auth.json` 整段），
本提案只新增「取出来喂给隔离子进程 + 用完吸收回来」的出口。

## 2. 隔离机制（已验证）

两个 provider 的路径层已认环境变量（`crates/providers/codex/src/paths.rs::codex_home`、
`crates/providers/claude/src/paths.rs::claude_home`），launcher 只需给子进程设变量。

| 目标 | 机制 | 结论 |
|---|---|---|
| Codex（全平台） | `CODEX_HOME=<私有目录>`，auth.json 落该目录，Codex CLI 自刷新 | ✅ |
| Claude / Linux | `CLAUDE_CONFIG_DIR=<私有目录>` + 写 `.credentials.json`；非账号内容链接回全局 `~/.claude` | ✅ |
| Claude / macOS OAuth | `CLAUDE_CONFIG_DIR` → 钥匙串 item 按目录哈希命名空间隔离；非账号内容链接回全局 `~/.claude` | ✅ |

### 2.1 macOS 钥匙串命名空间（关键，反编译 claude 2.1.177 确认）

Claude Code 在 macOS 上的钥匙串 item service 名按 config dir 哈希加后缀。反编译
claude 2.1.177 的 service 名构造器（minified 名 `oy`）与模块常量 `d8H="-credentials"`：

```js
function oy(H=""){
  let _=process.env.CLAUDE_SECURESTORAGE_CONFIG_DIR,
      q = _!==void 0 ? !_ : !process.env.CLAUDE_CONFIG_DIR,
      K = _!==void 0 ? _.normalize("NFC") : Y8(),    // 解析后的 config dir
      O = q ? "" : `-${sha256(K).hex.substring(0,8)}`;
  return `Claude Code${OAUTH_FILE_SUFFIX}${H}${O}`;
}
// OAuth 凭证 blob 用 oy(d8H) 即 oy("-credentials")，6 处调用
// account 维度 = WN() = $USER（按 /^[a-zA-Z0-9._-]+$/ 校验，不合法回退 "claude-code-user"）
```

**精确推导（实现直接照此）**：

```
service = "Claude Code" + OAUTH_FILE_SUFFIX + "-credentials" + suffix
  OAUTH_FILE_SUFFIX = ""（普通 claudeai 登录；"-custom-oauth" / "-local-oauth" 为 dev OAuth 模式）
  suffix = ""                              当 CLAUDE_CONFIG_DIR / CLAUDE_SECURESTORAGE_CONFIG_DIR 均未设
         = "-" + sha256(NFC(dir)).hex[:8]  当 CLAUDE_CONFIG_DIR 设为 dir（或 SECURESTORAGE 设为非空）
  dir = CLAUDE_SECURESTORAGE_CONFIG_DIR（若设）否则解析后的 CLAUDE_CONFIG_DIR
account = $USER（按上面正则清洗，非法 → "claude-code-user"）
```

- 不设 `CLAUDE_CONFIG_DIR` → suffix 空 → 全局 item `Claude Code-credentials`
  （subswap 现在 `lib.rs::CLAUDE_CODE_KEYCHAIN_SERVICE` 读的就是它，已在本机验证可读）。
- 设 `CLAUDE_CONFIG_DIR=/path/A` → item = `Claude Code-credentials-<hashA>`，与全局 item
  及其他目录天然不冲突。

**推论**：macOS 上靠 `CLAUDE_CONFIG_DIR` 即可隔离多个 OAuth 账号，Claude Code 在各自 item 里
正常自刷新。subswap 植入凭证时不能只写 `.credentials.json`（macOS 读钥匙串），而要按上面公式
算出命名空间 item 名，用 `/usr/bin/security add-generic-password -a $USER -s <service>` 写入。
注：`${a96}/.oauth_token`(fD5) 等文件路径属 `a96="/home/claude/.claude/remote"` 的远程/沙箱
执行环境，**与本地 macOS 持久化无关**，不要当作本地文件后端。

### 2.2 其他相关入口

- `CLAUDE_SECURESTORAGE_CONFIG_DIR`：单独重定向凭证存储目录（独立于 `CLAUDE_CONFIG_DIR`）；
  设为空串强制回退全局命名空间。
- `CLAUDE_CODE_OAUTH_TOKEN`：直接注入 token，完全不碰钥匙串、进程级隔离最干净。
  **代价**：二进制原话「short-lived and not auto-refreshed when passed via env var」——
  注入的是不自刷新的短期 token，只适合短任务，或由 subswap 每次启动注入新鲜 token。

### 2.3 共享 Claude 工作环境（关键，OpenConductor resume 依赖）

`subswap run claude <id>` 的目标不是给每个账号创建一套全新的 Claude 工作环境，而是**只隔离账号身份**：

- 隔离目录独立持有：`.credentials.json`、`.claude.json` / `.config.json`、`.subswap-api.json` 等账号相关文件。
  `.claude.json` 除 `oauthAccount` 外还须预置 `hasCompletedOnboarding: true`——
  claude 在该字段缺失时无论钥匙串里有无有效凭证都会运行首次引导（含「Select login method」）；
  该字段由 `materialize_isolated` → `mark_onboarding_complete` 写入。
- 链接回全局 `~/.claude`：`projects` / `plugins` / `skills` / `commands` / `hooks` / `file-history` / `todos`，以及全局已存在的 `sessions` / `transcripts` / 其它非账号条目。
- `settings.json` / `settings.local.json`：从全局复制并剥掉 subswap 管理的 API 账号 env（`ANTHROPIC_*`、`CLAUDE_CODE_*`），保留 permissions、hooks、其它用户设置。不能直接 symlink，否则全局 custom-API active 时会污染 OAuth 隔离账号。

这个不变量用于支持 OpenConductor 这类调度器：不同账号通过 `subswap run` 跑同一项目时，必须能共享 Claude Code 的 `projects` 会话历史，从而 `--resume <session>` 不因账号隔离而失效。

## 3. 核心约束：refresh token 一次性轮换（必须先解决）

> 沿用 [PROVIDER_KNOWLEDGE_BASE.md](../PROVIDER_KNOWLEDGE_BASE.md)「Refresh token 轮换」不变量：
> 原生客户端是 live token 的唯一轮换者；refresh token 刷一次旧的立即作废。

现状之所以成立是因为「全局只有一个活账号」。隔离后同一时刻有多个活账号，必须处理：

1. **账号独占 checkout 锁**。同一账号绝不能同时被两个隔离环境借走：两个 Claude Code 会从
   同一份 refresh token 各自轮换，必有一方被服务端作废 → `refresh token already used` 强制重登。
   借出即加锁，挡住第二次借用 **和** 全局 `swap`。
2. **daemon 保活避让**。`active_account_id()`（`lib.rs:335`）现靠全局 `~/.claude.json` 的
   `oauthAccount` 判断活账号从而跳过。隔离环境里的活账号分散在各私有目录、daemon 看不见，会把它当
   parked 去后台刷 → 同样作废。daemon 必须读一张「已 checkout 账号」表并跳过其中所有账号。
3. **会话退出后吸收回 FileStore**。隔离环境里 Claude Code 自刷新后，FileStore 副本会过期。
   subswap 须在会话结束（或下次复用该账号前）从私有目录 / 命名空间 item 读回新凭证写回 FileStore。

好处：launcher 模型下每个会话独占自己目录，不再抢写全局 live 文件，`capture-on-leave`
那套回灌体操在隔离路径上基本不需要——只保留「退出吸收」一处即可。

## 4. 命令面（与 swap 并存）

```
subswap run codex <id> [-- ...]   # ✅ 已实现：设 CODEX_HOME=<私有目录>
   ├─ export_auth_blob(<id>) 取 auth.json（active 优先 live，其余读 FileStore）
   ├─ Checkout::acquire：flock 独占锁（阻止同账号重复借用；崩溃自动释放）
   ├─ 物化 <data_dir>/envs/codex/<id>/：写 auth.json(0600)，best-effort 复制 config.toml
   ├─ 设 CODEX_HOME，spawn codex，持锁等待子进程
   └─ 退出：absorb_auth_blob 把轮换后的 auth.json 吸收回 FileStore，release
subswap run claude <id> [-- ...]  # ✅ 已实现：在 <data_dir>/envs/claude/<id>/ 物化私有目录
   ├─ macOS：按 §2.1 公式写命名空间钥匙串 item；Linux：写 .credentials.json
   ├─ 隔离目录内账号文件独立，非账号内容按 §2.3 链接 / 复制回全局 Claude 工作环境
   ├─ 设 CLAUDE_CONFIG_DIR=<私有目录>，exec claude，退出吸收
subswap shell <id>                # ✅ 已实现：导出好环境变量的子 shell，交互连跑多条命令
subswap env <id>                  # ✅ 已实现：打印 export 行，供 eval "$(subswap env ...)"
```

心智模型：`swap` = 全局切到某号；`run`/`shell` = 当前终端临时用某号、不动全局、可与别的终端并行。

## 5. 待决问题 / 风险

- **macOS item service 名**：§2.1 已对 claude 2.1.177 钉死精确公式。仍属**静态反编译结论、未实机实测**
  （安全实测需一个备用登录账号，避免轮换污染主账号）。上游改命名规则会破坏植入，实现须带版本兜底 /
  失败探测（植入后用 `security find-generic-password` 自检读回）。
- **私有目录里的明文凭证**：Linux `.credentials.json`、macOS 命名空间 item 都落在用户可读位置，
  权限须 `0600`，目录 `0700`；会话结束按策略清理或保留（待定）。
- **manual_only API 账号**：API 模式靠 `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` 进程级注入，
  天然适配隔离，不涉及轮换；但仍受 checkout 锁与现有 `manual_only` 语义约束。
- **崩溃 / 强杀导致锁泄漏**：已解决——checkout 用 flock，进程退出（含崩溃）OS 自动释放，无陈旧锁。
- **对「全局 active 账号」做隔离启动的轮换冲突（已做保护）**：`run` / `shell` / `env` 对全局 active
  账号会打印告警；手动 `swap` 会拒绝切到正在 checked-out 的账号；默认入口与 daemon 的 auto-swap 在同
  provider 存在 checked-out 账号时跳过本轮自动切换，避免全局写入与隔离会话抢同一份可轮换凭证。
- **daemon 避让已接线**：`keep_claude_tokens_alive` 每轮对每个 Claude 账号先查 `is_checked_out`，
  命中则跳过保活刷新，避免抢刷被隔离会话持有的账号 token。
- **`env`（eval 模式）的固有局限**：`eval "$(subswap env <id>)"` 设完环境变量后 subswap 即退出，
  **无法持锁、退出后不吸收轮换凭证**。只适合临时短用；长会话用 `run` / `shell`。命令会打印该告警。

## 6. 不变量影响（实现时同步 AGENTS.md）

- 「全局单活账号」假设被打破 → daemon 保活与 `active_account_id()` 须改为「跳过所有 checked-out 账号」。
- `Provider::activate` 仍只服务全局 swap；隔离启动走新路径（植入 + checkout），不复用 activate 的原地覆盖。
- 新增 `subswap run` / `shell` / `env` 子命令须遵守 CLI 既有约定（编号、`swap` 命名、`list_ordered`）。

# 账号环境隔离设计（subswap run / shell）

> 状态：**已实现并验证**——Codex / Claude / Kimi / OpenCode Go 隔离、`run`/`shell`/`env`、同账号并发会话、daemon 保活避让均已落地。Kimi / OpenCode 走共享引擎通用隔离，不必再为每个文件型 provider 改 `run.rs` 分发。
>
> 代码锚点：
> - `crates/core/src/checkout.rs`：序列号并发子目录 + `is_checked_out`（v0.3.25 起移除独占 flock）。
> - `crates/providers/codex`：`export_auth_blob` / `absorb_auth_blob`。
> - `crates/providers/claude`：`export_isolated_credentials` / `materialize_isolated` / `absorb_isolated` / `isolated_keychain_service`（公式 §2.1）。
> - `crates/cli/src/cmd/run.rs`：`run` / `shell` / `env`。
> - `crates/daemon/src/unix.rs`：`keep_claude_tokens_alive` 跳过 checked-out。
>
> 实机 2026-06-15：钥匙串植入公式（§2.1）正确，claude 2.1.177 可读命名空间 item；阻断点曾是 `hasCompletedOnboarding`（§2.3），已在 `materialize_isolated` 修复。

## 1. 目标

`swap` = 全局单活 + 原地覆盖。本能力另增：**启动子进程时把指定账号投影到私有目录，用环境变量让 CLI 只看该目录**——多终端并行不同账号、不动全局活账号。前提：`FileStore` 已有完整凭证；本路径只做「取出 → 喂隔离子进程 → 吸收回写」。

## 2. 隔离机制（已验证）

路径层认环境变量（`codex::paths::codex_home`、`claude::paths::claude_home`）；launcher 给子进程设变量即可。

| 目标 | 机制 | 结论 |
|---|---|---|
| Codex（全平台） | `CODEX_HOME=<私有目录>`，auth.json 落该目录，Codex CLI 自刷新 | ✅ |
| Kimi（全平台） | `KIMI_CODE_HOME=<私有目录>`，凭证落该目录 | ✅ |
| OpenCode Go（全平台） | `XDG_DATA_HOME=<私有目录>` 写 `opencode/auth.json`，并设 `OPENCODE_AUTH_CONTENT`（官方在此变量存在时忽略磁盘文件） | ✅ |
| Claude / Linux | `CLAUDE_CONFIG_DIR=<私有目录>` + 写 `.credentials.json`；非账号内容链接回全局 `~/.claude` | ✅ |
| Claude / macOS OAuth | `CLAUDE_CONFIG_DIR` → 钥匙串 item 按目录哈希命名空间隔离；非账号内容链接回全局 `~/.claude` | ✅ |

### 2.1 macOS 钥匙串命名空间（关键，反编译 claude 2.1.177 确认）

Claude Code 钥匙串 item service 名按 config dir 哈希加后缀。反编译 service 名构造器（minified `oy`）与常量 `d8H="-credentials"`：

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

- 不设 `CLAUDE_CONFIG_DIR` → 全局 item `Claude Code-credentials`（`lib.rs::CLAUDE_CODE_KEYCHAIN_SERVICE`）。
- 设 `CLAUDE_CONFIG_DIR=/path/A` → `Claude Code-credentials-<hashA>`，与全局及其他目录不冲突。

**推论**：macOS 靠 `CLAUDE_CONFIG_DIR` 即可隔离多 OAuth；植入须按公式写命名空间 item（`/usr/bin/security add-generic-password -a $USER -s <service>`），不能只写 `.credentials.json`。`${a96}/.oauth_token` 等属远程/沙箱路径，**与本地 macOS 持久化无关**。

### 2.2 其他相关入口

- `CLAUDE_SECURESTORAGE_CONFIG_DIR`：单独重定向凭证目录；空串强制回退全局命名空间。
- `CLAUDE_CODE_OAUTH_TOKEN`：进程级注入、不碰钥匙串；**不自刷新**（官方：short-lived），只适合短任务或每次启动注新鲜 token。

### 2.3 共享 Claude 工作环境（关键，OpenConductor resume 依赖）

`subswap run claude <id>` **只隔离账号身份**：

- 隔离目录独立：`.credentials.json`、`.claude.json` / `.config.json`、`.subswap-api.json` 等。
  `.claude.json` 除 `oauthAccount` 外须预置 `hasCompletedOnboarding: true`（缺则强制首次引导）；由 `materialize_isolated` → `mark_onboarding_complete` 写入。
- 链接回全局 `~/.claude`：`projects` / `plugins` / `skills` / `commands` / `hooks` / `file-history` / `todos`，以及已存在的 `sessions` / `transcripts` / 其它非账号条目。
- `settings.json` / `settings.local.json`：从全局复制并剥掉受管 API env（`ANTHROPIC_*`、`CLAUDE_CODE_*`），保留 permissions/hooks 等。禁止直接 symlink（全局 custom-API active 会污染 OAuth 隔离账号）。

不变量：不同账号 `subswap run` 同一项目须共享 `projects` 会话，使 `--resume <session>` 不因隔离失效。

## 3. 核心约束：refresh token 一次性轮换（必须先解决）

> 沿用 PROVIDER_KNOWLEDGE_BASE「Refresh token 轮换」：原生客户端是 live token 唯一轮换者；刷一次旧的立即作废。

隔离后多活账号须处理：

1. **同账号并发会话**（v0.3.25）。`Checkout::acquire` 不再独占 flock，改为单调序列号子目录 `<data_dir>/envs/<provider>/<id>/<seq>/`。
   **token 轮换并发风险**：多会话同时 refresh → 一方可能 `refresh token already used` 需重登——实践中短任务触发概率极低，可接受；`absorb_isolated` **无写时冲突检测**，最后一次覆盖胜出（先 absorb 的更新 token 可能被后者旧值覆盖）。全局 `swap`/自动切换不因活跃隔离会话拒绝/跳过；与同账号并发隔离风险相同，当前接受以保证全局切号始终可用。
2. **daemon 保活避让**。`active_account_id()` 靠全局 `~/.claude.json` 的 `oauthAccount` 判活账号；隔离活账号分散在私有目录，daemon 看不见会当 parked 后台刷 → 作废。须读「已 checkout 账号」表并跳过。
3. **会话退出后吸收回 FileStore**。隔离内自刷新后 FileStore 副本过期；须在会话结束（或下次复用前）从私有目录 / 命名空间 item 读回写 FileStore。

隔离路径上 `capture-on-leave` 回灌基本不需要——只保留退出吸收。

## 4. 命令面（与 swap 并存）

```
subswap run codex <id> [-- ...]   # ✅ 设 CODEX_HOME=<私有目录>
   ├─ export_auth_blob(<id>)（active 优先 live，其余 FileStore）
   ├─ Checkout::acquire：<data_dir>/envs/codex/<id>/<seq>/（Drop 清理；崩溃残留为近似指标）
   ├─ 物化：auth.json(0600)，best-effort 复制 config.toml
   ├─ 设 CODEX_HOME，spawn，等待
   └─ 退出：absorb_auth_blob → FileStore，release
subswap run claude <id> [-- ...]  # ✅ <data_dir>/envs/claude/<id>/
   ├─ macOS：§2.1 写命名空间 item；Linux：写 .credentials.json
   ├─ 账号文件独立；非账号按 §2.3 链接/复制
   ├─ 设 CLAUDE_CONFIG_DIR，exec，退出吸收
subswap shell <id>                # ✅ 导出环境变量的子 shell
subswap env <id>                  # ✅ 打印 export，供 eval
```

心智：`swap` = 全局切号；`run`/`shell` = 本终端临时用某号、不动全局、可并行。

## 5. 待决问题 / 风险

- **macOS item service 名**：§2.1 公式已对 claude 2.1.177 钉死，且 2026-06-15 实机验证可读。上游改命名仍会破坏植入 → 须版本兜底 / `security find-generic-password` 自检。
- **私有目录明文凭证**：Linux `.credentials.json`、macOS 命名空间 item → 权限 `0600`、目录 `0700`；会话结束清理或保留（待定）。
- **manual_only API 账号**：进程级 `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` 天然适配隔离；仍受 `manual_only`（不参与自动切换）。
- **崩溃残留 env 目录**：v0.3.25 无 flock 后，崩溃时 `Checkout::Drop` 不执行 → `<id>/<seq>/` 可能残留；`is_checked_out` 以数字名子目录存在为近似活跃指标 → 误判活跃直至手动清理（不影响全局 `swap`，但暂停该 Claude 账号 daemon 保活）。清理：删 `<data_dir>/envs/<provider>/<id>/` 下数字名子目录。
- **对全局 active 做隔离启动**：会告警；手动 `swap`/默认入口/daemon auto-swap 仍可在隔离期间切换。仅恰逢 refresh 轮换才可能互废 token；当前接受以保证全局切换可用。
- **daemon 避让已接线**：`keep_claude_tokens_alive` 每轮先查 `is_checked_out`，命中跳过。
- **`env`（eval）固有局限**：`eval "$(subswap env <id>)"` 后 subswap 退出 → **无法持锁、不吸收轮换**。只适合短用；长会话用 `run`/`shell`（命令会打告警）。

## 6. 不变量影响（实现时同步 AGENTS.md）

- 「全局单活」打破 → daemon Claude 保活须跳过 checked-out；全局手动/自动切换不因隔离阻断。
- `Provider::activate` 只服务全局 swap；隔离走植入 + checkout，不复用 activate 原地覆盖。
- `run` / `shell` / `env` 须遵守 CLI 约定（编号、`swap` 命名、`list_ordered`）。

<!-- 该文档整理/压缩于 2026-09-05 -->

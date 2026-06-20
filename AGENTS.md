# subswap · Agent 快速规则

> 全局规范仍适用；本文件只保留本项目最容易漏掉、最影响安全性的规则。文档索引见下方「文档导航」。

## 最高优先级

1. **功能新增或缺陷修复后，默认执行完整发布流程**（通用约束见全局规范「改动即发布新版本」）。
   本项目的具体落地步骤：
   - 按语义化版本提升 workspace 版本并同步 `Cargo.lock`。
   - 跑测试 / 构建 / release 构建。
   - 先覆盖安装本机 `subswap` / `subswapd`。
   - 重启 daemon，并验证版本与构建产物哈希。
   - 再提交 Git、创建并推送版本 tag、确认 GitHub Release 发布成功。
2. **修改代码前先用 GitNexus。**
   编辑函数 / 方法 / 类型前跑 `gitnexus_impact(repo: "subswap", direction: "upstream")`；
   HIGH / CRITICAL 先告警。提交前跑 `gitnexus_detect_changes(scope: "staged")`。
3. **工作区可能是脏的。**
   不回滚、不覆盖无关本地改动；提交时只 stage 本次相关文件。
4. **改完必须验证。**
   代码、配置、构建脚本、依赖改动后，自己跑对应测试 / build / smoke，不把验证交给用户。

## 项目不变量

- 手动 `subswap swap` 永远不依赖 quota 查询；网络坏、quota API 坏、token 过期时也要能切走。
- Claude 自定义 API 账号必须标记 `manual_only`：只能手动切入，active 时禁用自动换号，也不能成为自动候选；
  切回 OAuth 时必须恢复进入 API 模式前的 `settings.json.env` 受管字段。
- macOS 上读写 Claude Code 的 `Claude Code-credentials` keychain item **只能 fork `/usr/bin/security`**，
  禁止用 `keyring` crate（security-framework 原生 API）：keyring 写会把 item ACL 重置成「仅 subswap」，
  导致 Claude Code（也用 `security` 读）每次切换后反复弹授权框。详见
  [docs/troubleshooting/2026-06-11-claude-code-keychain-acl-poisoning.md](docs/troubleshooting/2026-06-11-claude-code-keychain-acl-poisoning.md)。
  - 测试隔离：集成测试**禁止触碰真实登录钥匙串**（否则 `cargo test` 在 macOS 弹授权框并改写本机凭证）。
    所有 `security` 读写认 `SUBSWAP_CLAUDE_KEYCHAIN_PATH` 环境变量重定向到一次性 keychain；
    `cli_surface.rs::isolated_subswap` 已统一设置它，新写的会激活 Claude OAuth 的集成测试也必须经它隔离。
- `subswap run claude` 的隔离 `.claude.json` 必须包含 `hasCompletedOnboarding: true`，
  否则 claude 无论钥匙串里有无有效凭证都会弹「Select login method」首次引导——
  由 `materialize_isolated` 调 `mark_onboarding_complete` 写入；改隔离物化流程时不得删除该调用。
  详见 [docs/design/ACCOUNT_ISOLATION_DESIGN.md](docs/design/ACCOUNT_ISOLATION_DESIGN.md) §2.3。
- `Provider::activate` 必须先写快照，任一目标写失败要回滚。
- refresh token 是一次性轮换：subswap 对 active 账号只读不刷，由原生客户端唯一轮换。
  `activate` 覆盖 live 文件前先 capture-on-leave 回灌 live 凭证进 owner 账号 store；
  daemon keepalive / `query_quota` 401 自愈只对 parked 账号刷新。daemon 每轮还做 capture-on-arrival
  (`reconcile_active_from_live`，只 live→store) 补「绕过 swap 离开」的缺口；refresh 回 `invalid_grant`
  时死 token 守卫止住反复刷的风暴并显示 `needs re-login`。细节见
  [docs/PROVIDER_KNOWLEDGE_BASE.md](docs/PROVIDER_KNOWLEDGE_BASE.md) 的「Refresh token 轮换」。
  **`capture_live_into_store` 绝不能用缺 refresh 的 live 快照覆盖 store 里有 refresh 的副本**（会把账号静默写死），
  两个 provider 各有守卫，改此逻辑前见
  [docs/troubleshooting/2026-06-18-live-capture-clobbers-refresh-token.md](docs/troubleshooting/2026-06-18-live-capture-clobbers-refresh-token.md)。
- 新 Provider 只能放在 `crates/providers/<id>`，再到 `AppContext::build()` 注册，并在默认入口同步本地 active。
- AutoSwap 默认阈值只改 `crates/core/src/defaults.rs::AUTO_SWAP_THRESHOLD`，并同步
  [docs/design/AUTO_SWAP_DESIGN.md](docs/design/AUTO_SWAP_DESIGN.md)。
- `async fn` 内不得直接做阻塞 IO；文件锁、`std::fs`、keyring 等必须包进 `tokio::task::spawn_blocking`。
- 写入 `registry.toml` 的 `Option<T>` 字段必须加
  `#[serde(skip_serializing_if = "Option::is_none")]`，避免 TOML null 报错。
- CLI 子命令、Rust 标识符、英文文案统一用 `swap`，不要用 `switch`。
- `swap` / `rm` 的数字编号必须与默认入口显示顺序一致，统一走 `AppContext::list_ordered()`。
- 跨模块调优参数走 `crates/core/src/settings.rs::current()`，不要在 provider / cli 里硬编码阈值、窗口、百分比。
- 不得用高频 quota / usage 请求模拟限流触发；必须保守退避，避免请求风暴和风控风险。
  Anthropic usage 端点限流极严（~每账号每分钟 1 次），**禁止手动 `curl` 连发去"复现"**——会打爆桶、
  污染判断。查询前先走缓存节流：缓存比 `settings.quota.min_refresh_interval_ms`(默认 90s) 新就复用、
  不打端点；daemon 与 CLI 共用 `quota_cache.json`，两条路径都要尊重。**429 ≠ token 失效**，三种「查不出」
  的区分与处理见 [docs/PROVIDER_KNOWLEDGE_BASE.md](docs/PROVIDER_KNOWLEDGE_BASE.md) 的「Usage 接口异常状态码」；
  根因与修复历史见
  [docs/troubleshooting/TROUBLESHOOTING_INDEX.md](docs/troubleshooting/TROUBLESHOOTING_INDEX.md)。

## 代码风格

- 代码注释、doc comment 用中文。
- 用户可见输出、错误文本、tracing message、Cargo description 用英文且简洁。
- 成功路径尽量短；冗余 hint 只在失败时出现。
- 公共 API 加中文 doc comment；trait 不暴露 keyring 等具体实现类型。

## 常用验证命令

```bash
cargo check --workspace
cargo test --workspace
cargo build --workspace
# 升版本号后必须先同步 lock,否则下面的 --locked 构建会报 "cannot update the lock file"。
cargo update --workspace --offline
cargo build --locked --release -p subswap-cli -p subswap-daemon
```

## 本机发布 / 冒烟

```bash
install -m 755 target/release/subswap ~/.local/bin/subswap
install -m 755 target/release/subswapd ~/.local/bin/subswapd

shasum -a 256 target/release/subswap target/release/subswapd \
  ~/.local/bin/subswap ~/.local/bin/subswapd

pkill -f 'subswap __daemon' 2>/dev/null || true
pkill -f 'subswapd' 2>/dev/null || true
SUBSWAP_AUTO_DAEMON=1 ~/.local/bin/subswap
~/.local/bin/subswap --version
pgrep -af 'subswap __daemon|subswapd' || true
```

## 目录速记

```text
crates/core/              数据模型、Provider trait、CredentialStore、路径、策略
crates/cli/               subswap 二进制与默认入口
crates/daemon/            subswapd 后台轮询、自动切换、Claude token 保活
crates/providers/codex/   Codex / ChatGPT Provider
crates/providers/claude/  Claude / Anthropic Provider
docs/                     中文项目文档
```

## 文档导航

| 文档 | 用途 |
|---|---|
| [docs/PROVIDER_KNOWLEDGE_BASE.md](docs/PROVIDER_KNOWLEDGE_BASE.md) | 改 Provider 切换、认证、额度或自定义 API 逻辑前必读 |
| [docs/design/ARCHITECTURE.md](docs/design/ARCHITECTURE.md) | 架构、模块边界、数据流 |
| [docs/design/AUTO_SWAP_DESIGN.md](docs/design/AUTO_SWAP_DESIGN.md) | 改自动切换候选筛选、阈值、manual_only、防抖/振荡刹车，或排查「默认入口渐进式重判 / 一次 subswap 多次切换 / 连跑结果不同 / 卡在耗尽号 / 账号间无限横跳(A→B→A 振荡)」前必读 |
| [docs/design/PREWARM_DESIGN.md](docs/design/PREWARM_DESIGN.md) | 窗口预热提案 |
| [docs/design/ACCOUNT_ISOLATION_DESIGN.md](docs/design/ACCOUNT_ISOLATION_DESIGN.md) | 做 `subswap run`/`shell` 账号环境隔离、改 checkout 锁 / daemon 避让 / macOS 钥匙串命名空间，或排查 Claude resume 看不到会话 / 隔离目录不共享 projects、settings、plugins 前必读 |
| [docs/CONFIG.md](docs/CONFIG.md) | `config.toml` 字段与热加载 |
| [docs/CLI.md](docs/CLI.md) | 改 CLI 命令面、交互向导或 `subswapd` 辅助进程前必读 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | 里程碑进度 |
| [docs/troubleshooting/TROUBLESHOOTING_INDEX.md](docs/troubleshooting/TROUBLESHOOTING_INDEX.md) | **排查任何故障 / 报错 / 异常行为前必读**：先在此查有无同类前例，避免重新 debug 已解决的问题（9 篇记录：keychain ACL 中毒、refresh token 覆写、429 vs invalid_grant、TOML null 等）；纯功能开发或改配置时可跳过；是本项目全部故障排查的权威来源 |

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **subswap** (1580 symbols, 4128 relationships, 137 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> Index stale? Run `node .gitnexus/run.cjs analyze` from the project root — it auto-selects an available runner. No `.gitnexus/run.cjs` yet? `npx gitnexus analyze` (npm 11 crash → `npm i -g gitnexus`; #1939).

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows. For regression review, compare against the default branch: `detect_changes({scope: "compare", base_ref: "main"})`.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `rename` which understands the call graph.
- NEVER commit changes without running `detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/subswap/context` | Codebase overview, check index freshness |
| `gitnexus://repo/subswap/clusters` | All functional areas |
| `gitnexus://repo/subswap/processes` | All execution flows |
| `gitnexus://repo/subswap/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->

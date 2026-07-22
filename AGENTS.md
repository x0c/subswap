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
   - GitHub Release publish 后 `update-homebrew.yml` 会**自动**更新 `x0c/homebrew-tap` 的 formula，无需手动操作。
     详见 [docs/OPERATIONS_GUIDE.md](docs/OPERATIONS_GUIDE.md) §「Homebrew Tap 自动更新」。
2. **修改代码前先查调用链。**
   编辑函数 / 方法 / 类型前用 codebase-memory-mcp 的 `trace_path` 查清调用者 / 被调用者，评估影响面后再动手。
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
- refresh token 是一次性轮换，active 账号默认只读不刷；允许自愈的唯一例外是**复用原生客户端官方协调机制**，
  绝不能由 subswap 自创一套互不相认的锁或并行抢刷：Codex 通过官方 app-server 查询/刷新，Kimi 只在能
  识别并持有当前版本官方跨进程锁时刷新，Cursor active 账号只重读 live、不刷新。
  `activate` 覆盖 live 文件前先 capture-on-leave 回灌 live 凭证进 owner 账号 store；
  parked 账号按各 Provider 的串行化边界刷新；Cursor 必须使用 subswap 跨进程锁。daemon 每轮还做 capture-on-arrival
  (`reconcile_active_from_live`，只 live→store) 补「绕过 swap 离开」的缺口；refresh 被上游拒绝时必须有
  死 token 守卫止住反复刷的风暴并显示 `needs re-login`，Kimi/Cursor 的跨进程守卫只保存 refresh token
  SHA-256 指纹、不保存 secret。细节见
  [docs/PROVIDER_KNOWLEDGE_BASE.md](docs/PROVIDER_KNOWLEDGE_BASE.md) 的「Refresh token 轮换」。
  **`capture_live_into_store` 绝不能用缺 refresh 的 live 快照覆盖 store 里有 refresh 的副本**（会把账号静默写死），
  各 Provider 都必须保留守卫，改此逻辑前见
  [docs/troubleshooting/2026-06-18-live-capture-clobbers-refresh-token.md](docs/troubleshooting/2026-06-18-live-capture-clobbers-refresh-token.md)。
- 新 Provider 只能放在 `crates/providers/<id>`，再到 `AppContext::build()` 注册，并在默认入口同步本地 active。
  Cursor 这类凭证位于 SQLite、切换还要协调 GUI 生命周期的 Provider 必须独立实现 `Provider`，不能硬塞进
  文件型 JSON 共享引擎；Cursor 也不支持 `subswap run/shell/env` 隔离运行。细节见
  [docs/PROVIDER_KNOWLEDGE_BASE.md](docs/PROVIDER_KNOWLEDGE_BASE.md) 的「Cursor」与
  [docs/design/ARCHITECTURE.md](docs/design/ARCHITECTURE.md) 的「扩展新 Provider」。
- 文件型（凭证是本地一个 JSON blob、靠覆盖文件切换）provider 的切换机制统一在 `crates/providers/common`
  （`FileBlobProvider<A>` 引擎）。新增此类 provider（如未来第三个）**只写一个 `FileBlobRuntime` 实现**
  （路径、元数据解析、刷新、usage 查询等差异点），在 `AppContext::build()` 的 provider 列表注册一行，
  若要支持 `subswap run/shell/env` 隔离运行再把它塞进 `isolated: HashMap<&str, Arc<dyn IsolatedProvider>>`
  表（`FileBlobRuntime` 有隔离能力时自动获得 `IsolatedProvider` blanket impl）——**隔离分发**
  （`run.rs` 内的 materialize/absorb/env_vars/native_cli 查表逻辑）因此不用改。但 `run.rs` 的
  `normalize_provider` 仍需加一行别名匹配（把用户输入的 provider 名解析成规范 id，纯文本解析，
  查表机制吸收不了）；`login.rs` **必须**新增一个该 provider 专属的 match 分支——登录流程从未做成
  通用查表（Codex 走 `codex login` 子进程、Claude 走 `claude auth login --claudeai`、Kimi 是纯导入
  已登录凭证，语义各不相同），每个新 provider 都要写自己的登录分支。历史数据兼容用两个可选 hook：
  `store_field()`（凭证仓库里存 blob 的字段名，默认 `"blob"`）与 `dedup_extra_key()`（`registry.toml
  extra` 里去重键的字段名，默认 `"dedup_key"`）——仅当迁移一个已有存量账号数据、且旧字段名与默认值
  不同的 provider（如 Codex 分别覆盖成 `"auth_json"`/`"chatgpt_account_id"`）时才需要覆盖，全新
  provider（如 Kimi）用默认值即可。Claude 因 macOS 钥匙串 + API 账号特殊逻辑，不在此引擎上，`run.rs`
  保留其专用分支。
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
crates/providers/common/  文件型 OAuth 账号切换共享引擎（FileBlobProvider/FileBlobRuntime/IsolatedProvider）
crates/providers/codex/   Codex / ChatGPT Provider（adapter，跑在 common 引擎上）
crates/providers/claude/  Claude / Anthropic Provider（keychain 特化，独立于 common 引擎）
crates/providers/kimi/    Kimi / Moonshot Provider（adapter，跑在 common 引擎上）
crates/providers/cursor/  Cursor Provider（SQLite + GUI 生命周期特化，独立于 common 引擎）
docs/                     中文项目文档
```

## 文档导航

| 文档 | 用途 |
|---|---|
| [docs/PROVIDER_KNOWLEDGE_BASE.md](docs/PROVIDER_KNOWLEDGE_BASE.md) | 改、评审、分析或排查 Provider 切换、认证、额度、refresh token、自定义 API、Claude/Codex/Kimi/Cursor 本地激活状态、原生客户端并发协调、或文件型 OAuth 切换共享引擎（`crates/providers/common`）前必读 |
| [docs/design/ARCHITECTURE.md](docs/design/ARCHITECTURE.md) | 改、评审或分析 workspace 分层、Provider 抽象、核心数据流、凭证文件布局、新 Provider 接入前必读 |
| [docs/design/AUTO_SWAP_DESIGN.md](docs/design/AUTO_SWAP_DESIGN.md) | 改、评审或排查自动切换候选筛选、阈值、manual_only、防抖/振荡刹车、daemon token 保活，或排查「默认入口渐进式重判 / 一次 subswap 多次切换 / 连跑结果不同 / 卡在耗尽号 / 账号间无限横跳(A→B→A 振荡)」前必读 |
| [docs/design/PREWARM_DESIGN.md](docs/design/PREWARM_DESIGN.md) | 设计、评审或实现窗口预热、预热阈值、预热通知与自动切换协同时必读 |
| [docs/design/ACCOUNT_ISOLATION_DESIGN.md](docs/design/ACCOUNT_ISOLATION_DESIGN.md) | 改、评审、分析或排查 `subswap run`/`shell`/`env` 账号环境隔离、checkout 锁、daemon 避让、macOS 钥匙串命名空间、Claude resume 会话共享前必读 |
| [docs/CONFIG.md](docs/CONFIG.md) | 改、评审或排查 `config.toml` 字段、热加载、默认阈值、轮询间隔、quota 查询节流和配置生效问题前必读 |
| [docs/CLI.md](docs/CLI.md) | 改、评审、分析或排查 CLI 命令面、Provider 登录/导入语义、默认入口额度输出、`subswapd` 辅助进程、账号环境隔离命令或 Cursor 不支持隔离运行的边界前必读 |
| [docs/OPERATIONS_GUIDE.md](docs/OPERATIONS_GUIDE.md) | 改、评审或排查本地构建、测试、release 构建、本机覆盖安装、daemon 冒烟、CI/Release 发布流程、Homebrew tap formula 更新机制或 `HOMEBREW_TAP_TOKEN` 配置前必读 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | 规划、评审或同步里程碑范围、已完成能力和后续功能优先级前必读 |
| [docs/troubleshooting/TROUBLESHOOTING_INDEX.md](docs/troubleshooting/TROUBLESHOOTING_INDEX.md) | **排查任何故障 / 报错 / 异常行为前必读**：先在此查有无同类前例，避免重新 debug 已解决的问题（10 篇记录：keychain ACL 中毒、refresh token 覆写、429 vs invalid_grant、TOML null、Codex 用量 401 但 CLI 能正常用等）；纯功能开发或改配置时可跳过；是本项目全部故障排查的权威来源 |

## 领域地图（doc-init）

<!-- 覆盖度复核基线：2026-06-29 · 源码指纹 扫描 91 文件 / Rust 45 / 5 子模块 · 基线提交 73cb6d8 -->

| 领域 | 入口锚点 |
|------|---------|
| Provider 账号、凭证、额度与激活 | `crates/providers/`、`crates/providers/common/`、`crates/core/src/provider.rs` |
| CLI 命令面与默认入口 | `crates/cli/src/main.rs`、`crates/cli/src/cmd/` |
| 自动切换策略与 daemon 协同 | `crates/core/src/auto_policy.rs`、`crates/daemon/` |
| 账号环境隔离运行 | `crates/cli/src/cmd/run.rs`、`crates/core/src/checkout.rs` |
| 运行时配置与默认参数 | `crates/core/src/settings.rs`、`crates/core/src/defaults.rs` |
| 架构分层与新 Provider 接入 | `crates/core/`、`crates/providers/` |
| 窗口预热设计 | `docs/design/PREWARM_DESIGN.md` |
| 运行、验证与发布流程 | `.github/workflows/`、`Cargo.toml` |
| 故障排查知识网络 | `docs/troubleshooting/TROUBLESHOOTING_INDEX.md` |

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
- `Provider::activate` 必须先写快照，任一目标写失败要回滚。
- refresh token 是一次性轮换：subswap 对 active 账号只读不刷，由原生客户端唯一轮换。
  `activate` 覆盖 live 文件前先 capture-on-leave 回灌 live 凭证进 owner 账号 store；
  daemon keepalive / `query_quota` 401 自愈只对 parked 账号刷新。细节见
  [docs/PROVIDER_KNOWLEDGE_BASE.md](docs/PROVIDER_KNOWLEDGE_BASE.md) 的「Refresh token 轮换」。
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
| [docs/design/AUTO_SWAP_DESIGN.md](docs/design/AUTO_SWAP_DESIGN.md) | 改自动切换候选筛选、阈值、manual_only 行为前必读 |
| [docs/design/PREWARM_DESIGN.md](docs/design/PREWARM_DESIGN.md) | 窗口预热提案 |
| [docs/design/ACCOUNT_ISOLATION_DESIGN.md](docs/design/ACCOUNT_ISOLATION_DESIGN.md) | 做 `subswap run`/`shell` 账号环境隔离、改 checkout 锁 / daemon 避让 / macOS 钥匙串命名空间前必读 |
| [docs/CONFIG.md](docs/CONFIG.md) | `config.toml` 字段与热加载 |
| [docs/CLI.md](docs/CLI.md) | 改 CLI 命令面、交互向导或 `subswapd` 辅助进程前必读 |
| [docs/ROADMAP.md](docs/ROADMAP.md) | 里程碑进度 |
| [docs/troubleshooting/2026-05-28-claude-config-dir-parent-pollution.md](docs/troubleshooting/2026-05-28-claude-config-dir-parent-pollution.md) | 排查 Claude 配置父目录污染、路径误判或配置目录隔离问题 |
| [docs/troubleshooting/2026-05-28-toml-null-serialization.md](docs/troubleshooting/2026-05-28-toml-null-serialization.md) | 排查 TOML 序列化写出 null、配置保存异常前阅读 |
| [docs/troubleshooting/2026-05-29-daemon-keyutils-session-isolation.md](docs/troubleshooting/2026-05-29-daemon-keyutils-session-isolation.md) | 排查 daemon 与 keyutils session 隔离、凭据读取失败前阅读 |
| [docs/troubleshooting/2026-05-29-macos-keychain-prompts.md](docs/troubleshooting/2026-05-29-macos-keychain-prompts.md) | 排查 macOS Keychain 弹窗、凭据访问提示或权限体验前阅读 |
| [docs/troubleshooting/2026-06-06-filestore-credential-backend.md](docs/troubleshooting/2026-06-06-filestore-credential-backend.md) | 排查 filestore 凭据后端、跨平台凭据保存行为前阅读 |
| [docs/troubleshooting/2026-06-08-codex-refresh-token-already-used.md](docs/troubleshooting/2026-06-08-codex-refresh-token-already-used.md) | 排查 Codex refresh token already used、令牌刷新竞态前阅读 |
| [docs/troubleshooting/2026-06-11-claude-code-keychain-acl-poisoning.md](docs/troubleshooting/2026-06-11-claude-code-keychain-acl-poisoning.md) | 排查 macOS 反复弹「security wants to access "Claude Code-credentials"」、改 Claude keychain 读写前必读 |

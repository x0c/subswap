# subswap - Claude、Codex 和 ChatGPT 账号切换器

语言： [English](README.md) | 简体中文 | [日本語](README.ja.md) | [한국어](README.ko.md)

subswap 是一个 Rust CLI，用于管理 Claude Code、Anthropic Claude、OpenAI Codex CLI 和 ChatGPT 的多个 AI 订阅账号。它可以导入本地登录状态，将凭证存入系统 keyring，查询额度窗口，并在用量超过可配置阈值时手动或自动切换当前账号。

它既可以作为 Claude 账号切换器、Codex 账号管理器、ChatGPT 额度追踪器，也可以作为统一的多 Provider 订阅切换工具。

## 功能

- **Claude Code 和 Codex CLI 多账号切换**：无需重新登录即可切换当前账号。
- **Claude Code 自定义 API 端点**：通过终端交互向导添加 DeepSeek 或其他 Anthropic 兼容端点，并像 Claude 账号一样双向切换。
- **额度感知状态**：在可用时查看 Provider 额度窗口，例如 Claude 5h / 7d 用量，以及 Codex / ChatGPT 使用数据。
- **自动账号切换**：后台 daemon 可以在用量超过配置阈值后切走当前账号。
- **不依赖网络的手动切换**：即使额度 API 失败、token 过期或网络不可用，`subswap swap` 仍可工作。
- **文件凭证存储**：凭证保存在应用数据目录下仅 owner 可读的 `0600` 文件中，旧 keyring 数据首次读取时自动迁移。
- **基于 Provider 的架构**：Claude / Anthropic 与 Codex / ChatGPT 位于独立 crate 中，因此可以在不改变 core 策略的情况下添加新的 AI Provider。

## 支持的客户端

| Provider | 本地客户端 | subswap 管理内容 |
|---|---|---|
| Claude / Anthropic | Claude Code (`~/.claude`) | OAuth 凭证、自定义 API 端点、当前账号文件、5h / 7d 额度、token 保活 |
| Codex / ChatGPT | Codex CLI (`~/.codex`) | `auth.json` 透传、当前账号文件、ChatGPT 使用量查询 |

## 常见场景

- 在多个 Claude Pro、Claude Max、ChatGPT Plus 或 ChatGPT Team 席位之间切换。
- 当前账号达到使用限制时，随时切换到备用 AI 订阅。
- 开始长时间编码会话前，检查各账号用量。
- 用一个 CLI 统一管理 Claude 和 ChatGPT 账号切换。

## 状态

| 里程碑 | 范围 | 状态 |
|---|---|---|
| M1 | workspace + core trait/model + minimal CLI | done |
| M2 | Claude provider: keyring-backed swap, 5h/7d quota, best-effort token refresh | done |
| M3 | Codex provider: opaque auth.json passthrough, atomic write, tolerant wham/usage parsing | done |
| M4 | `subswapd` daemon: periodic poll + auto-swap + Claude token keepalive + zero-config auto-spawn | done |

## 为什么需要它

如果你同时付费使用多个 AI 订阅，可能会遇到：

- Claude Pro 用量耗尽后，希望无需重新登录就切到 ChatGPT；
- 持有两个 ChatGPT 席位，希望用一行命令切换当前账号；
- 希望查看所有账号在各个窗口（5h / 7d）中的剩余额度。

subswap 会把每个账号保存到系统 keyring（Keychain / Credential Manager / secret-service），对所有读取同一套本地凭证文件的客户端原子切换当前账号，并且永远不会因为网络问题阻塞手动切换；额度查询只作为参考信息。

## 安装

使用 Homebrew：

```bash
brew install x0c/tap/subswap
```

或先添加 tap，再按名称安装：

```bash
brew tap x0c/tap
brew install subswap
```

从源码安装需要 Rust 1.80+。

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --path crates/cli
subswap --help
```

也可以直接从 Git 安装：

```bash
cargo install --git https://github.com/x0c/subswap --path crates/cli
```

## 快速开始

```bash
# default: sync local active accounts, fetch quotas, auto-swap if past threshold,
# then print a one-screen status. Run this whenever you want to know what's up.
subswap

# manually swap to a specific account (escape hatch — never depends on the network)
subswap swap alice@example.com
# disambiguate when the same id exists under multiple providers:
subswap swap claude/alice@example.com

# 交互式添加 DeepSeek 或其他 Claude Code 兼容 API
subswap add-api
# 自定义 API 只能手动切换，不参与自动换号
subswap swap deepseek

# remove an account from the registry and the keyring
subswap rm alice@example.com

# environment self-check (client files, keyring, config dirs)
subswap doctor
```

只要你至少登录过一次 Claude Code / Codex CLI，第一次运行 `subswap` 时就会自动从 `~/.claude` 和 `~/.codex` 发现账号。

第一次执行 `subswap` 会在非 macOS 的 Unix 平台启动一个分离的后台 daemon。它会轮询额度并在后台自动切换账号，同时保持 Claude OAuth token 新鲜，避免长期闲置账号在刚切换过去时立即返回 401。macOS 需要显式 opt-in，因为后台进程访问 Keychain 容易触发额外授权弹窗：导出 `SUBSWAP_AUTO_DAEMON=1` 即可启用自动拉起。daemon 是单实例（文件锁），并且可以安全终止：`pkill -f 'subswap __daemon'` 或 `pkill subswapd`。如需完全禁用，导出 `SUBSWAP_NO_DAEMON=1`。

## 设计不变量

这些约束很关键，贡献代码前值得了解：

1. **`swap` 永远不依赖额度查询。** 如果 API 不可用、keyring 无法访问或 token 过期，手动切换仍必须尝试激活本地账号。
2. **密钥不进入 registry 元数据，快照仅 owner 可读。** OAuth/API 密钥保存在 `0600` 凭证文件中；自定义 API active 时，Claude Code 还要求 API Key 写入 `~/.claude/settings.json`，subswap 会原子保存并在切回 OAuth 时恢复。
3. **切换是原子的，并且可以回滚。** 每次 `activate` 在修改任何文件之前都会把快照写入 `state_dir/snapshots/<ts>/`；任一写入失败都会回滚。
4. **新增 Provider = 新增 `crates/providers/<id>` crate + 在 `cli/src/main.rs::AppContext::build()` 中注册一行。** `core` 中不放 Provider 特定逻辑。
5. **自动切换阈值集中管理且可配置。** 编译期默认值位于 `crates/core/src/defaults.rs`，运行时配置可以覆盖它。

更多内容见 [`docs/`](docs/)（中文内部协作文档）。

## 对比

| 工具类型 | 关注点 | subswap 的区别 |
|---|---|---|
| 单 Provider 账号切换工具 | 一次只面向一个上游 | subswap 在同一套 Provider 抽象下支持 Claude 和 Codex / ChatGPT |
| 额度看板 | 只展示用量 | subswap 还可以在额度窗口耗尽时激活另一个本地账号 |
| 手动登录/退出 | 一次只处理一个账号 | subswap 将已注册账号保存在 keyring 中，并原子切换本地活动文件 |

## FAQ

### `subswap swap` 会调用额度 API 吗？

不会。手动切换是逃生通道，永远不依赖额度查询。即使上游 API 不可用或 token 过期，`subswap swap claude/alice@example.com` 也会尝试激活该本地账号。

### token 存在哪里？

token 和 refresh token 存在应用数据目录下仅 owner 可读的凭证文件中。自定义 API active 时，Claude Code 还要求 API Key 出现在 `~/.claude/settings.json`；切换快照同样收紧为 `0600`。

### 自定义 API 会参与自动换号吗？

不会。自定义 API 是 `manual_only`：subswap 不会自动选中它；它处于 active 时，自动换号也完全停用。手动切回 OAuth 账号时，会恢复进入 API 模式前的 Claude Code 设置。

### 这只适用于 Claude 吗？

不是。首批支持的 Provider 是 Claude / Anthropic 和 Codex / ChatGPT。core crate 暴露 Provider trait，因此未来的 AI 订阅 Provider 可以作为独立 crate 加入。

## GitHub topics

发布后推荐设置的仓库 topics：

`claude-code`, `codex-cli`, `chatgpt`, `anthropic`, `openai`, `account-switcher`, `quota-tracker`, `ai-tools`, `rust-cli`, `keyring`, `automation`

## 目录结构

```
crates/
  core/                # data model, Provider trait, CredentialStore, paths
  cli/                 # `subswap` binary
  daemon/              # `subswapd` binary
  providers/
    claude/            # Claude / Anthropic provider
    codex/             # Codex / ChatGPT provider
```

## 贡献

欢迎提交 issues 和 PR。注意：

- `docs/` 和 `AGENTS.md` 中的内部文档使用中文；代码注释使用中文；所有用户可见内容（CLI 文案、错误消息、tracing 日志、crate description）使用英文。
- 提交 PR 前请运行 `cargo check --workspace` 和 `cargo test --workspace`。

## License

MIT — 见 [`LICENSE`](LICENSE)。

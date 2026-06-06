# subswap · Agent 规范

> 全局规范见 `~/.claude/CLAUDE.md` 与 `~/.claude/AGENTS.md`，此处只补充项目特定内容。

## 项目定位

subswap 是多 AI 订阅账号的统一切换与额度管理工具，一期支持 Codex / ChatGPT 与 Claude / Anthropic，
设计上通过 Provider 抽象支持未来扩展其他订阅。

- 目标：统一多 Provider 账号切换 + 额度查询 + 阈值/限流双触发的自动切换。

## 技术栈

- Rust 1.80+，Cargo workspace。
- 异步：tokio + reqwest(rustls)。
- CLI：clap v4 derive；TUI（M2 之后）：ratatui + crossterm。
- 凭证：keyring crate（macOS Keychain / Windows Credential Manager / Linux secret-service）。
- 序列化：serde + serde_json + toml。
- 日志：tracing + tracing-subscriber。

## 目录约定

```
subswap/
├── Cargo.toml                  # workspace 根
├── crates/
│   ├── core/                   # 数据模型、Provider trait、CredentialStore、路径
│   ├── cli/                    # `subswap` 二进制
│   ├── daemon/                 # `subswapd` 二进制(CLI 默认入口自动拉起)
│   └── providers/
│       ├── codex/              # Codex / ChatGPT Provider
│       └── claude/             # Claude / Anthropic Provider
├── docs/
│   ├── OVERVIEW.md             # 文档索引（必读）
│   ├── design/                 # 架构与方案
│   └── troubleshooting/        # 故障排查记录
└── tests/                      # 集成测试
```

## 不变量（写代码前必读）

1. **手动 `swap` 命令不得依赖额度查询**。`Provider::activate` 必须能在 `query_quota` 全部失败时仍正常工作。
   动机：用户可能在 quota 接口不可用、网络异常、密钥过期等情境下仍需要切走当前账号。
   **推论**：activate 路径上的 token 预刷新是 best-effort，失败只 warn 不阻塞。
2. **敏感字段一律走 keyring**。registry.toml、审计日志、快照里都不允许出现明文 token / refresh_token。
3. **多客户端切换必须可回滚**。`activate` 前先写快照到 `state_dir/snapshots/<ts>/`，任一目标写失败即回滚。
4. **新增 Provider = 新建 `crates/providers/<id>` crate + 在 `AppContext::build()` 注册 + 在 `sync_local_active()` 加 import_active**。
   不要把 Provider 特定逻辑写到 core 里。
5. **自动切换默认阈值只以 `defaults.rs` 为准**。运行时实际值由 `<config_dir>/config.toml` `[auto_swap].threshold` 覆盖，
   缺失时回落到 `crates/core/src/defaults.rs::AUTO_SWAP_THRESHOLD`。改默认值只动 `defaults.rs` 一处，
   并同步更新 docs/design/AUTO_SWAP_DESIGN.md。配置字段定义见 docs/CONFIG.md。
6. **`async fn` 不得直接做阻塞 IO**。文件锁（fs2）、std::fs 同步读写、keyring 等阻塞调用必须包在
   `tokio::task::spawn_blocking` 里。daemon 周期轮询 + 多 Provider 并发 query_quota 时,堵塞 worker 会让整体卡顿。
7. **任何会被写入 `registry.toml` 的 `Option<T>` 字段必须加 `#[serde(skip_serializing_if = "Option::is_none")]`**。
   原因：`serde_json` 把 `None` → `null`，而 TOML 规范不支持 null，否则保存时报 `unsupported unit type`。
   详见 docs/troubleshooting/2026-05-28-toml-null-serialization.md。
8. **CLI 子命令、Rust 标识符、英文文案统一用 `swap`，不要用 `switch`**。中文「切换」不动。
   `swap` / `rm` 接受数字编号引用（如 `subswap swap 3`），编号由 `AppContext::list_ordered()` 生成，
   必须与默认入口渲染的顺序严格一致 —— 增改这两处其一时务必同步检查另一处，否则用户会切到错误的账号。
9. **所有跨模块数值调优参数走 `crates/core/src/settings.rs::current()` 读取**，源文件是
   `<config_dir>/config.toml`（缺失 / 字段缺失时回落 `defaults.rs`）。不允许在 provider 或 cli 里硬编码
   阈值、时间窗口、百分比。新增一个调优参数：先在 `defaults.rs` 加常量 → `settings.rs::Settings` 加字段
   并接到对应 `Default impl` → `docs/CONFIG.md` 文档化 → 调用点用 `settings::current().group.field`。
   daemon 每轮、CLI 每次启动都会 `reload_from_file()`，运行期改 `config.toml` 即时生效。详见
   ARCHITECTURE.md §5.5 与 docs/CONFIG.md。
10. **不得用高频请求模拟限流触发**。任何 quota / usage 轮询、daemon 后台保活、未来 429 上报机制都必须保守：
    遵守上游服务条款、避免请求风暴、失败后退避；不要为了更快切换而增加封号/风控风险。
11. **功能新增或缺陷修复后默认执行完整发布流程**。按语义化版本提升 workspace 的 patch/minor/major 版本并
    同步 `Cargo.lock`，完成测试与 release 构建后提交 Git、创建并推送版本 tag、确认远端 release 发布成功，
    再覆盖安装本机 `subswap` / `subswapd` 并重启 daemon；最后用 `subswap --version` 和构建产物哈希验证。
    除非用户明确限制范围，不得只完成其中一部分。

## 代码风格

- **代码注释（`//` `///`）保持中文**，沿用全局规范。
- **用户可见输出一律英文，且尽量精简**：
  - `println!` / `eprintln!` / `clap` 的 `about` / `long_about` / `help` / `arg` doc。
  - `anyhow!` / `bail!` / `.context(...)` 抛出的错误。
  - `subswap_core::Error` 各变体 `#[error("...")]` 的 Display 文本。
  - `tracing::{info,warn,error,debug,trace}!` 的 message（用户开 `RUST_LOG` 时可见）。
  - Cargo.toml 的 `description`（会出现在 crates.io / GitHub）。
- UI 优先「能不说话就不说话」：成功路径短一行（如 `swap → claude/alice`），冗余 hint 仅在失败时出现。
- 错误信息英文，但保留足够上下文供排查（provider id、account id、操作名、底层错误）。
- 公共 API 加 doc comment（中文）；私有函数视需要而定。
- 避免在 trait 中暴露具体类型（如 keyring::Entry），用 String/PathBuf 等基础类型。

> 历史背景：早期 CLI 文案全中文，开源后改为英文。`docs/` 与本 AGENTS.md 仍保持中文，面向项目内协作。

## 编译与本地运行

```bash
# 全量检查
cargo check --workspace

# 调试构建
cargo build --workspace

# 运行 CLI
cargo run -p subswap-cli -- --help
cargo run -p subswap-cli -- doctor
cargo run -p subswap-cli
```

## 文档导航

详见 [docs/OVERVIEW.md](docs/OVERVIEW.md)。

| 文档 | 路径 | 用途 |
|---|---|---|
| Provider 知识库 | docs/PROVIDER_KNOWLEDGE_BASE.md | 各 Provider 上游接口、文件结构、坑点 |
| 架构设计 | docs/design/ARCHITECTURE.md | 模块划分、依赖关系、扩展机制 |
| 自动切换设计 | docs/design/AUTO_SWAP_DESIGN.md | 触发策略、降级路径 |
| 窗口预热提案 | docs/design/PREWARM_DESIGN.md | 无头 hi 预热 5h 窗口（提案/未实现，#10 豁免） |
| 运行时配置 | docs/CONFIG.md | `config.toml` 字段表、热加载、风控约束 |
| 故障排查记录 | docs/troubleshooting/YYYY-MM-DD-*.md | 时序归档 |

新增「以前从未出现过的文档类型」时，需同时更新本文「文档导航」表与 `docs/OVERVIEW.md`。

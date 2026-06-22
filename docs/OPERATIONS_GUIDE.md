# subswap 运行与验证 Guide

## 文档定位

本 Guide 覆盖 subswap 的本地构建、测试、release 构建、本机覆盖安装、daemon 冒烟和 CI/Release 验证口径。不覆盖 Provider 业务规则、自动切换决策、账号隔离机制或历史故障；这些内容分别进入对应领域文档和故障排查索引。

## 验证结论摘要

本次 doc-init 已实际执行并验证：

| 验证项 | 命令/动作 | 结果 | 结论 |
|---|---|---|---|
| 项目扫描 | `project_inventory.py --root .` | 成功 | Rust workspace、CI、release、测试入口可被自动发现 |
| Git 弱信号扫描 | `git_history_miner.py --root .` | 成功 | 最近 46 条提交可用，只作为热点和风险线索 |
| 文档导航初检 | `doc_nav_lint.py --root .` | 发现 1 个 error、1 个 warning | `CLAUDE.md` 必须规范成单行；隔离设计文档顶部自我导航需改写 |
| 文档导航复检 | `doc_nav_lint.py --root .` | 成功 | 无 error、无 warning；`领域地图（doc-init）` 已写入 |
| Rust 编译检查 | `cargo check --workspace` | 成功 | 全 workspace 通过 |
| Rust 测试 | `cargo test --workspace` | 成功 | 全 workspace 通过，CLI 集成测试与自动切换策略测试均通过 |

## 进程启动矩阵

| 启动对象 | 命令 | 入口 | 端口 | 外部依赖 | 本地可关闭项 | 置信度 |
|---|---|---|---|---|---|---|
| CLI 默认入口 | `subswap` | `crates/cli/src/main.rs` | 无 | 本机 Claude/Codex 登录文件、subswap 配置目录 | 可设 `SUBSWAP_NO_DAEMON=1` 禁止默认入口拉起 daemon | 已验证来源：README/AGENTS/源码 |
| CLI 子命令 | `subswap --help`、`subswap swap`、`subswap run`、`subswap doctor` | `crates/cli/src/main.rs` | 无 | 视子命令读取本地账号文件或钥匙串 | 测试中使用临时 HOME、临时客户端目录和一次性 keychain | 已验证来源：CLI 集成测试 |
| daemon | `subswap __daemon` 或 `subswapd` | `crates/daemon/src/main.rs`、`crates/daemon/src/unix.rs` | 无 | 本地账号 store、Provider usage API、配置文件 | macOS 默认不自动拉起；可用 `SUBSWAP_NO_DAEMON=1` 禁止 | 已验证来源：README/AGENTS/源码 |
| release 二进制 | `target/release/subswap`、`target/release/subswapd` | release profile | 无 | Rust target、Linux 需 `libdbus-1-dev pkg-config` | 本机安装前可只跑 release build | 已验证来源：release workflow |

## 本地启动前检查

- Rust 工具链：workspace 声明 `rust-version = "1.80"`，CI 使用 stable。
- Linux 依赖：CI 在 Linux 安装 `libdbus-1-dev pkg-config`，否则 keyring 相关依赖可能链接失败。
- daemon 副作用：本地运行默认入口时如只做测试，优先设置 `SUBSWAP_NO_DAEMON=1`，避免留下后台进程。
- 真实账号隔离：新增会触发 Claude OAuth 或 Codex 登录状态的集成测试时，必须沿用 `crates/cli/tests/cli_surface.rs::isolated_subswap` 的隔离环境，特别是 `SUBSWAP_CLAUDE_KEYCHAIN_PATH`。
- macOS 钥匙串：测试用一次性 keychain，不得触碰真实 `Claude Code-credentials` 登录钥匙串。

## 启动命令

常规开发验证：

```bash
cargo check --workspace
cargo test --workspace
cargo build --workspace
```

release 构建验证：

```bash
cargo update --workspace --offline
cargo build --locked --release -p subswap-cli -p subswap-daemon
```

本机覆盖安装和 daemon 冒烟：

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

## 存活验证

| 场景 | 验证方式 | 成功信号 | 失败时先看 |
|---|---|---|---|
| CLI 是否可执行 | `subswap --help` | 输出当前命令面，不出现已删除命令 | `crates/cli/src/main.rs` 和 `docs/CLI.md` 是否同步 |
| 空环境默认入口 | 临时 HOME 下运行 `subswap` 并设置 `SUBSWAP_NO_DAEMON=1` | 提示没有账号，不探测真实账号 | 集成测试里的隔离环境是否缺项 |
| release 产物 | `~/.local/bin/subswap --version` | 版本与 workspace 版本一致 | 是否忘记安装新产物或同步 lock |
| daemon | `pgrep -af 'subswap __daemon|subswapd'` | 需要冒烟时可看到进程 | macOS 是否未设置 `SUBSWAP_AUTO_DAEMON=1` |
| 本机安装一致性 | `shasum -a 256 ...` | target 和 `~/.local/bin` 产物哈希一致 | 是否安装了旧构建产物 |

## 常见启动失败信号

| 现象 | 优先怀疑 | 验证方式 | 下一步 | 证据来源 |
|---|---|---|---|---|
| `cargo build --locked` 提示 lock file 需要更新 | 升版本或依赖后没有同步 `Cargo.lock` | 看错误里是否出现 cannot update lock file | 先跑 `cargo update --workspace --offline` 再重试 locked build | AGENTS |
| Linux CI 编译 keyring 相关依赖失败 | 缺少 D-Bus 开发包和 pkg-config | 对比 `.github/workflows/ci.yml` | 安装 `libdbus-1-dev pkg-config` | CI |
| 集成测试弹 macOS 钥匙串授权框 | 测试触碰真实登录钥匙串 | 检查是否设置 `SUBSWAP_CLAUDE_KEYCHAIN_PATH` | 改用 `isolated_subswap` 或补齐隔离环境变量 | `cli_surface.rs` |
| 本地默认入口留下后台进程 | 没有设置 `SUBSWAP_NO_DAEMON=1` | `pgrep -af 'subswap __daemon|subswapd'` | 测试场景设置 `SUBSWAP_NO_DAEMON=1`，需要冒烟时再显式启动 | README/测试 |
| daemon 冒烟后版本仍旧 | `~/.local/bin` 未覆盖或 shell 命中旧路径 | `command -v subswap`、`subswap --version`、哈希对比 | 重新安装 release 产物并确认 PATH | AGENTS |

## Homebrew Tap 自动更新

`x0c/homebrew-tap` 的 `Formula/subswap.rb` 由 `.github/workflows/update-homebrew.yml` 全自动维护，**无需手动操作**。

### 工作原理

1. `release.yml` 的 `publish` 作业把 draft 切成 published（`gh release edit ... --draft=false`）
2. GitHub 触发 `release: published` 事件，`update-homebrew.yml` 自动执行
3. workflow 从 release assets 下载各平台的 `.sha256` 文件，用 Python 渲染新 formula，通过 GitHub API PUT 到 `homebrew-tap` 仓库

### 用到的 Secret

`HOMEBREW_TAP_TOKEN`（已在 `x0c/subswap` 仓库 Actions Secrets 里设置）：用 `gh auth token`（`repo` scope 的 OAuth token）写入，可 push 到同账号下的 `homebrew-tap` 仓库。

若 token 过期导致 workflow 失败，重新设置方法：

```bash
gh auth token | gh secret set HOMEBREW_TAP_TOKEN --repo x0c/subswap
```

### 用户安装命令

```bash
brew install x0c/tap/subswap   # 一步安装
# 或
brew tap x0c/tap && brew install subswap
# 升级
brew upgrade subswap
```

## 通用改动验证套路

- 改 CLI 命令面：先跑 `cargo test --workspace`，再看 `subswap --help` 输出；删除或新增命令时同步 `docs/CLI.md`。
- 改自动切换策略：跑 `cargo test --workspace`，重点关注 `crates/core/tests/auto_policy_integration.rs`；同步 `docs/design/AUTO_SWAP_DESIGN.md`。
- 改 Provider 凭证、额度或激活逻辑：跑全 workspace 测试；涉及 Claude/Codex live capture、refresh token、quota 429 时同步 `docs/PROVIDER_KNOWLEDGE_BASE.md` 和对应故障索引。
- 改隔离运行：跑 CLI 集成测试；macOS 钥匙串相关验证必须使用一次性 keychain；同步 `docs/design/ACCOUNT_ISOLATION_DESIGN.md`。
- 改配置字段或默认值：同步 `docs/CONFIG.md`；确认默认值只从 `crates/core/src/defaults.rs` 或 settings 入口读取。
- 改 release 产物或版本：跑 locked release build，安装到本机，验证版本与哈希，再走 tag 和 GitHub Release。

## 未确认项

- 本次 doc-init 未真实执行 release 构建、本机覆盖安装或 daemon 冒烟；本次文档改动已用文档 lint、`cargo check --workspace` 和 `cargo test --workspace` 验证。
- Windows release 由 workflow 打包，但项目 README 明确 Windows 未测试；不要把 Windows 视为本地高置信支持平台。

<!-- 该文档由 doc-init 生成于 2026-06-20；定位：AI 修改 subswap 运行、验证、发布流程前的快速参考文档 -->

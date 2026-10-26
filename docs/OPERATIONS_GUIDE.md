# subswap 运行与验证 Guide

本地构建、测试、release 构建、本机覆盖安装、daemon 冒烟和 CI/Release 验证口径。不覆盖 Provider 业务规则、自动切换决策、账号隔离机制或历史故障。

## 进程启动矩阵

| 启动对象 | 命令 | 入口 | 端口 | 外部依赖 | 本地可关闭项 | 置信度 |
|---|---|---|---|---|---|---|
| CLI 默认入口 | `subswap` | `crates/cli/src/main.rs` | 无 | 本机 Claude/Codex/Kimi/Cursor 登录状态、subswap 配置目录 | 可设 `SUBSWAP_NO_DAEMON=1` 禁止默认入口拉起 daemon；Windows 本就无 daemon | 已验证来源：README/AGENTS/源码 |
| CLI 子命令 | `subswap --help`、`subswap swap`、`subswap run`、`subswap doctor` | `crates/cli/src/main.rs` | 无 | 视子命令读取本地账号文件或钥匙串 | 测试中使用临时 HOME、临时客户端目录和一次性 keychain | 已验证来源：CLI 集成测试 |
| 只读状态快照 | `subswap status [--json]` | `crates/cli/src/cmd/status.rs` | 无 | 仅读 `registry.toml`、`quota_cache.json`、`request_metrics.json`、daemon PID | 不构造 Provider、不启动子进程、不访问网络、不写状态 | 已验证来源：`status_readonly` 集成测试 |
| daemon | `subswap __daemon` 或 `subswapd` | `crates/daemon/src/main.rs`、`crates/daemon/src/unix.rs` | 无 | 本地账号 store、Provider usage API、配置文件 | macOS 默认不自动拉起；可用 `SUBSWAP_NO_DAEMON=1` 禁止 | 已验证来源：README/AGENTS/源码 |
| release 二进制 | `target/release/subswap`、`target/release/subswapd` | release profile | 无 | Rust target、Linux 需 `libdbus-1-dev pkg-config` | 本机安装前可只跑 release build | 已验证来源：release workflow |

## 本地启动前检查

- Rust 工具链：workspace 声明 `rust-version = "1.80"`，CI 使用 stable。
- Linux 依赖：CI 在 Linux 安装 `libdbus-1-dev pkg-config`，否则 keyring 相关依赖可能链接失败。
- daemon 副作用：本地运行默认入口时如只做测试，优先设置 `SUBSWAP_NO_DAEMON=1`，避免留下后台进程。
- Windows 只发布 `subswap.exe`，不构建 Unix-only 的 `subswapd`；CLI 与五个 Provider 由 Windows CI 实测。
- 真实账号隔离：新增会触发 Claude OAuth、Codex 登录状态或 Cursor 命令行钥匙串的集成测试时，必须沿用 `crates/cli/tests/cli_surface.rs::isolated_subswap` 的隔离环境，特别是 `SUBSWAP_CLAUDE_KEYCHAIN_PATH` 与 `SUBSWAP_CURSOR_KEYCHAIN_PATH`。
- macOS 钥匙串：测试用一次性 keychain，不得触碰真实 `Claude Code-credentials` 或 `cursor-access-token` / `cursor-refresh-token` 登录钥匙串。

## 三平台测试隔离

CLI 集成测试统一经 `isolated_subswap` 构造临时环境，不能只改 `HOME` 或 XDG 目录：Windows 的系统目录解析不遵循 XDG 覆盖，而且 Cursor 的平台默认数据库路径也不受 `HOME` 或 `SUBSWAP_HOME` 控制。

| 隔离对象 | 覆盖方式 | 约束 |
|---|---|---|
| subswap 配置、数据、状态、缓存 | `SUBSWAP_HOME` | 指向每个测试独占的绝对临时目录；相对路径直接报错 |
| Claude、Codex、Kimi、OpenCode 原生目录 | `CLAUDE_CONFIG_DIR`、`CODEX_HOME`、`KIMI_CODE_HOME`、`SUBSWAP_OPENCODE_HOME` | 全部指向同一个测试临时根下的不同子目录；OpenCode 测试绝不能回落到真实 `~/.local/share/opencode` |
| Cursor 原生状态数据库 | `SUBSWAP_CURSOR_STATE_DB_PATH` | 指向绝对临时路径下的 `state.vscdb`；文件可以尚不存在，禁止回退探测真实用户数据库 |
| macOS Claude / Cursor 命令行钥匙串 | `SUBSWAP_CLAUDE_KEYCHAIN_PATH`、`SUBSWAP_CURSOR_KEYCHAIN_PATH` | 使用同一份一次性 keychain，禁止读取或写入真实登录钥匙串 |
| 后台进程 | `SUBSWAP_NO_DAEMON=1` | 普通集成测试不留下 daemon；专门冒烟时再显式开启 |

`SUBSWAP_HOME` 的目录映射和便携运行边界以 [CONFIG.md](CONFIG.md) 为准。新增任何会探测原生客户端登录状态的 CLI 测试时，必须复用统一 helper，不得手工拼一套不完整的覆盖。

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

快速开发安装（仅 `subswap` CLI，不含 daemon）：

```bash
make install   # = cargo build --release + install subswap 到 ~/.local/bin
```

> 限制：`make install` 只安装 `subswap` 二进制，**不安装 `subswapd`**；
> 需要 daemon 冒烟或完整发布时，走下方完整流程。

本机覆盖安装和 daemon 冒烟：

```bash
install -m 755 target/release/subswap ~/.local/bin/subswap
install -m 755 target/release/subswapd ~/.local/bin/subswapd
```

先确认 `command -v subswap` 指向刚装上的那一份。若 `~/.cargo/bin` 排在 `~/.local/bin` 前面，只更新后者时，直接敲 `subswap` 仍会跑到旧的 `cargo install` 产物（开发机上曾因此一直用 0.3.25，列表里整段没有 Cursor）。这种情况要把新文件同步覆盖 PATH 里真正命中的那一份，或拿掉旧入口。

GitHub Release 的 Linux 包用 `cargo zigbuild` 把符号地板钉在 **GLIBC 2.28**（与 runner 自带 glibc 无关），覆盖 Ubuntu 20.04+ / Debian 10+ / RHEL 8+。发布流水线会用 `objdump` 校验产物不超过该地板；若校验失败，说明有人改回了在较新 Ubuntu 上直接 `cargo build`。本机开发仍用普通 `cargo build --release`；覆盖安装时不要拿旧的、按本机 2.39 编出来的二进制冒充 Release 产物。

```bash
shasum -a 256 target/release/subswap target/release/subswapd \
  ~/.local/bin/subswap ~/.local/bin/subswapd
# 用字符类避免 `pkill -f` 匹配并终止承载本段命令的 shell。
pkill -f '[s]ubswap __daemon' 2>/dev/null || true
pkill -f '[s]ubswapd' 2>/dev/null || true
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
| 只读状态面 | `subswap status --json` | `schema_version=1`、账号字段齐全、`generated_at` 变化但 `cached_at` 不变、`request_metrics` 结构稳定 | 是否误走默认入口/Provider 构造，或读到了真实客户端状态 |

## 只读状态面验证

`subswap status --json` 面向 GUI / widget / 脚本，必须能在没有任何 daemon、没有网络、没有原生客户端运行的情况下反复调用。完整约定见 [CLI.md](CLI.md)「只读状态快照（`status`）」。

### 隔离回归

```bash
cargo test -p subswap-cli --test status_readonly
cargo test -p subswap-core --test quota_collection
```

`status_readonly` 使用临时 `SUBSWAP_HOME`、临时原生客户端路径、一次性 Claude/Cursor keychain，并在运行前后对整个 app 根目录做内容快照。它同时用假原生客户端与本地监听器证明：`status` 不启动 Provider 进程、不建立网络连接；重复读取不改变 registry、quota cache、coordination 目录、metrics 文件或 daemon PID。空 home 用例还会断言执行后连 `config/data/state/cache` 目录都不会出现。

`quota_collection` 用假 Provider 和显式临时 cache 路径验证：成功结果写回、缓存命中保留原始 `observed_at`、并发采集合并为一次请求、取消后仍保留请求下限、超时/401 写入失败退避。测试不安装全局配置、不碰真实账号目录。

`crates/core/src/paths.rs` 的单元测试验证只读解析不创建目录、正常解析仍创建四类目录，并拒绝相对 `SUBSWAP_HOME`。

### 真实路径核对

先在隔离或真实环境运行：

```bash
subswap status --json
```

检查：

1. `generated_at` 是本次快照时间；每个账号的 `cached_at` 是原始额度观测时间。连续执行两次 `status`，`generated_at` 应变化，`cached_at`、`fetch_state`、`quotas` 不应因读取而被刷新。
2. `fetch_state` 能区分 `ready` / `stale` / `failed` / `missing`；已 reset 的窗口不出现在 `quotas` 中，但账号与原始 `cached_at` 仍保留。`manual_only` 账号单独标记，不因没有额度而被误判成失败。
3. 运行前后比较 `<cache>/quota_cache.json`、`<config>/registry.toml`、`<data>/request_metrics.json` 与 `<data>/state/subswapd.pid` 的哈希；只读快照不得改动它们。也不得出现新的 `*.lock` 或 `quota_collection/` 文件。
4. `request_metrics.available` 反映指标文件是否存在；`totals.requests.http_start/http_complete`、`native_rpc_*` 与 `collections.*` 分开计数。指标只覆盖 subswap 自己实际发出的请求与原生 RPC；**不能**用这些数字声称看到了官方客户端内部的 HTTP 流量。
5. 指标文件缺失或损坏时，`status` 仍应成功并输出 `available=false`；不能为读取指标而创建目录、锁文件或修复文件。

真实采集后的计数变化只能归因于 subswap 实际执行的那次采集。默认入口、daemon 或手动 `collect_quota` 可能让 `http_*` / `collections.*` 增长；`status` 本身不得让任何计数增长。

## 常见启动失败信号

| 现象 | 优先怀疑 | 验证方式 | 下一步 | 证据来源 |
|---|---|---|---|---|
| `cargo build --locked` 提示 lock file 需要更新 | 升版本或依赖后没有同步 `Cargo.lock` | 看错误里是否出现 cannot update lock file | 先跑 `cargo update --workspace --offline` 再重试 locked build | AGENTS |
| Linux CI 编译 keyring 相关依赖失败 | 缺少 D-Bus 开发包和 pkg-config | 对比 `.github/workflows/ci.yml` | 安装 `libdbus-1-dev pkg-config` | CI |
| Linux Release 安装依赖时 APT 镜像瞬态失败 | 镜像正在同步或包列表暂时不可用 | release 日志会提示清理包列表并在 10s、20s 后重试 | 等待内置 3 次有界重试；第 3 次仍失败时按真实基础设施故障排查，不继续无限重跑 | release workflow |
| 集成测试弹 macOS 钥匙串授权框 | 测试触碰真实登录钥匙串 | 检查是否设置 `SUBSWAP_CLAUDE_KEYCHAIN_PATH` 与 `SUBSWAP_CURSOR_KEYCHAIN_PATH` | 改用 `isolated_subswap` 或补齐隔离环境变量 | `cli_surface.rs` |
| 本地默认入口留下后台进程 | 没有设置 `SUBSWAP_NO_DAEMON=1` | `pgrep -af 'subswap __daemon|subswapd'` | 测试场景设置 `SUBSWAP_NO_DAEMON=1`，需要冒烟时再显式启动 | README/测试 |
| daemon 冒烟后版本仍旧 | `~/.local/bin` 未覆盖或 shell 命中旧路径 | `command -v subswap`、`subswap --version`、哈希对比 | 重新安装 release 产物并确认 PATH | AGENTS |

### Linux Release 的 APT 有界重试

Release workflow 在 Linux runner 上把 `apt-get update` 与 `libdbus-1-dev pkg-config` 安装视为一次完整尝试，最多执行 3 次。前两次失败后分别等待 10 秒、20 秒，并清理包列表与 APT 缓存再试；第 3 次仍失败则立即让构建失败。该机制只吸收镜像同步、网络抖动等瞬态故障，不掩盖包名错误、权限问题或持续性基础设施故障。

## Homebrew Tap 自动更新

`x0c/homebrew-tap` 的 `Formula/subswap.rb` 由 `.github/workflows/update-homebrew.yml` 全自动维护，**无需手动操作**。

发布后仍建议实际确认 `update homebrew formula` 作业是否运行成功、tap 里的 formula 是否已升到新版本。

### 工作原理

关键约束：`release.yml` 用默认 GitHub token（`github.token`）发布 Release，而 GitHub 的递归防护
**不会**让 `GITHUB_TOKEN` 制造的 `release: published` 事件再触发下游 workflow。因此 formula 更新
**不能**依赖 `release: published`（历史上 v1.1.0 及更早都是发布后手动补跑的）。现在改为在同一次
release run 内直接调用：

1. `release.yml` 创建 draft 时由 GitHub 自动生成 Release notes；维护者在发布前补充用户可见的限制、升级动作或人工验证说明，禁止再以一行自动文本代替版本说明。
2. 所有公开承诺平台的构建（Linux x86/ARM、macOS Intel/Apple Silicon、Windows x86）都必须成功；任一个失败时 draft 不得发布。
3. Windows 资产上传到 draft 后，`verify-windows-installer` 会用该次 tag 真实运行 `install.ps1`、校验 SHA-256 并检查安装后的版本号；这是发布前门槛，不用历史版本安装烟雾替代。
4. `publish` 仅在上述门槛成功后把 draft 切成 published（`gh release edit ... --draft=false`）。
5. `publish` 成功后，`release.yml` 的 `homebrew` 作业以 `workflow_call`（`uses: ./.github/workflows/update-homebrew.yml`、`secrets: inherit`）在同一次 run 内调用 formula 更新，绕开递归防护。
6. `update-homebrew.yml` 从 Release assets 下载各平台的 `.sha256` 文件，用 Python 渲染新 formula，通过 GitHub API PUT 到 `homebrew-tap` 仓库。所有 formula 更新共享一个串行队列；写入前重读当前文件，遇到并发 `409 Conflict` 时有界重试，并拒绝让旧 tag 覆盖已发布的新版本。相邻 tag 可以连续发布，但最终 formula 必须保持指向最高版本。

`update-homebrew.yml` 另保留 `workflow_dispatch`（手动补跑某个 tag：`gh workflow run update-homebrew.yml -f tag=vX.Y.Z`）
与 `release: published`（仅人工在 Release 页面手动发布时兜底）两个入口。

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

## Windows 一键安装

公开安装入口：

```powershell
irm https://raw.githubusercontent.com/x0c/subswap/main/install.ps1 | iex
```

脚本从最新 GitHub Release 下载 Windows zip，安装到当前用户的 LocalAppData 程序目录并更新用户 `PATH`。
改脚本后必须在 Windows CI 的临时安装目录真实跑一遍，再执行 `subswap --version`；常规 CI 保留已发布历史版本的回归烟雾，Release workflow 还必须对本次 draft tag 真实安装并核版本。仅做 PowerShell 语法检查不算验收。
Windows release 只有 CLI，文档和安装脚本都不得暗示包含后台 daemon。

## 通用改动验证套路

- 改 CLI 命令面：先跑 `cargo test --workspace`，再看 `subswap --help` 输出；删除或新增命令时同步 `docs/CLI.md`。
- 改自动切换策略：跑 `cargo test --workspace`，重点关注 `crates/core/tests/auto_policy_integration.rs`；同步 `docs/design/AUTO_SWAP_DESIGN.md`。
- 改 Provider 凭证、额度或激活逻辑：跑全 workspace 测试；涉及 Claude/Codex live capture、refresh token、quota 429 时同步 `docs/PROVIDER_KNOWLEDGE_BASE.md` 和对应故障索引。
- 改隔离运行：跑 CLI 集成测试；macOS 钥匙串相关验证必须使用一次性 keychain；同步 `docs/design/ACCOUNT_ISOLATION_DESIGN.md`。
- 改应用目录解析或原生客户端路径探测：跑三平台 CLI 集成测试，确认 `SUBSWAP_HOME` 与各客户端专属覆盖仍完整隔离真实用户状态；同步 `docs/CONFIG.md` 和本 Guide。
- 改配置字段或默认值：同步 `docs/CONFIG.md`；确认默认值只从 `crates/core/src/defaults.rs` 或 settings 入口读取。
- 改 release 产物或版本：跑 locked release build，安装到本机，验证版本与哈希，再走 tag 和 GitHub Release；发布前确认每个 README 承诺平台都有资产与校验和、Windows draft 安装验收通过、Release notes 可供用户阅读。
- **工具链一致性**：本机可能同时存在 Homebrew Rust 与 rustup。`cargo fmt` / `clippy` 默认走 PATH 里的那一个，而 CI 用 rustup stable；rustfmt 小版本会改变换行，导致本地通过、CI 失败。发版前用 `PATH="$HOME/.cargo/bin:$PATH" cargo --version` 对齐 CI，再跑 `cargo fmt --all --check` 与 `cargo clippy --workspace --all-targets -- -D warnings`。
- **失败的 tag 不移动**：tag 已推送而 release CI 失败时，不要 `git tag -f` 或强推；修复后提升 patch 版本重新发版，旧 tag 与 draft 保留为失败历史。发布 CI 通过后仍要下载实际资产核验。

### 额度窗口时长契约（2026-10-25）

窗口顺序与标签由真实时长决定，不再依赖 `five_hour` / `seven_day` / `month` 名单；完整契约见 [CLI.md](CLI.md)「额度窗口排序与标签」。

- `QuotaWindow::Timed(<秒>)` 序列化成时长 token（`20d` / `3h` / `90m` / `45s`）；Codex、Kimi 的上游窗口长度经 `QuotaWindow::from_seconds` 映射，新增长度不会退化成 `custom`。
- 验证：`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all --check` 全绿；release 安装到 `~/.local/bin` 后 daemon 已重启，默认入口仍是 5h → 7d → mo。
- 用 `SUBSWAP_HOME` 临时 home 构造含 `20d` / `3h` 的 `quota_cache.json`：`subswap status` 与 `subswap status --json` 分别输出 `3h · five_hour · seven_day · 20d · month · first_party_models` 与对应 token，证明非标准长度按秒插入而不是排在最后。Apple 端消费该快照的实际画面见 SubswapApple `docs/OPERATIONS_GUIDE.md`。
- 新增 Provider 时：上游给了窗口长度就必须走 `QuotaWindow::from_seconds`；只有完全拿不到长度才允许 `Custom`，否则客户端无法排序或标注。

### 发布后资产核验

- 下载全部 `.sha256` 与对应资产，逐个校验；不要只看 workflow 绿色。
- Windows 的 `.sha256` 由 PowerShell `Set-Content` 生成，行尾是 CRLF；macOS/Linux 的 `shasum -c` 会把 `\r` 当成文件名的一部分，误报 `No such file or directory`。用 `tr -d '\r'` 规范化后再 `shasum -c`，或直接对资产计算哈希并与文件内的 64 位值比较；不要据此判定资产损坏。长期修正：让 Windows 打包步骤输出 LF 校验文件。

Shared optional acceptance procedures now live in the agent-friendly-cli skill ([Optional acceptance procedures](~/.config/agentsync/skills/agent-friendly-cli/references/verification.md#optional-acceptance-procedures-observed-contracts-not-mandates)): hermetic helper, fake-scope honesty, handshake checks, binary-path verification, eval sets. Product routine stays here.

## 未确认项

- 本次 doc-init 未真实执行 release 构建、本机覆盖安装或 daemon 冒烟；本次文档改动已用文档 lint、`cargo check --workspace` 和 `cargo test --workspace` 验证。
- Windows 的本机手工冒烟仍依赖 CI 环境；当前支持结论来自三平台测试矩阵、Windows release 构建与一键安装链路，daemon 明确不在 Windows 支持范围内。

<!-- 该文档整理/压缩于 2026-09-05 -->

## Release notes

- [v1.16.6](releases/v1.16.6.md): quota windows ordered by real duration, duration tokens for non-standard lengths, and Codex/Kimi duration preservation.
- [v1.16.5](releases/v1.16.5.md): `status --json --no-metrics` for frequent GUI polling and the documented pipe-capacity contract; companion Subswap Apple client refresh deadlock fixed alongside it.
- [v1.16.4](releases/v1.16.4.md): empty-success cadence correction, Linux quota-refresh lock safety, Windows test determinism, portable lock-contention classification, and the complete shared quota collection, adaptive scheduling, read-only snapshots and request metrics delivered across the v1.16.0–v1.16.3 drafts.
- [v1.16.3](releases/v1.16.3.md): superseded by v1.16.4; it passed release CI but still re-collected empty successful readings at the request floor.
- [v1.16.2 draft](releases/v1.16.2.md): same behavior as v1.16.3; not publicly published because release CI rejected the committed formatting under the current stable rustfmt.
- [v1.16.1 draft](releases/v1.16.1.md): Windows strict-build correction and the original quota-collection notes; not publicly published because release CI found Linux lock safety and Windows test-timing defects fixed in v1.16.4.
- [v1.16.0 draft](releases/v1.16.0.md): original quota-collection notes; not publicly published because Windows validation rejected a Unix-only import. The corrected public delivery is v1.16.4.

## CI and Release failure badges

Keep CI and Release checks enabled and fix their failing steps. A working local binary does not prove that the latest source passes formatting, lint, cross-platform tests, or installer verification. The README badges report workflow outcomes, not local application health.

Release must validate the selected tag through the same CI workflow before building assets. All builds and the current Windows installer verification must succeed before publication. Do not use `always()` with a condition that checks only builds: that bypasses a failed installer dependency.

Draft installer verification must explicitly select the release database ID and expected version, authenticate to the release API, and download draft assets through their API URLs with `Accept: application/octet-stream`. Public tag/download URLs are unsuitable for draft verification. Normal installation must continue to reject drafts. Verify the installed version exactly. Re-running a published release must not turn it back into a draft or overwrite its assets.

References: [GitHub job dependencies](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-jobs), [release API](https://docs.github.com/en/rest/releases/releases), [asset downloads](https://docs.github.com/en/rest/releases/assets).

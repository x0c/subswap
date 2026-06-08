# subswap · Agent 快速规则

> 全局规范仍适用；本文件只保留本项目最容易漏掉、最影响安全性的规则。文档索引见
> [docs/OVERVIEW.md](docs/OVERVIEW.md)。

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
| [docs/PROVIDER_KNOWLEDGE_BASE.md](docs/PROVIDER_KNOWLEDGE_BASE.md) | Provider 上游接口、文件结构、坑点 |
| [docs/design/ARCHITECTURE.md](docs/design/ARCHITECTURE.md) | 架构、模块边界、数据流 |
| [docs/design/AUTO_SWAP_DESIGN.md](docs/design/AUTO_SWAP_DESIGN.md) | 自动切换触发与降级策略 |
| [docs/design/PREWARM_DESIGN.md](docs/design/PREWARM_DESIGN.md) | 窗口预热提案 |
| [docs/CONFIG.md](docs/CONFIG.md) | `config.toml` 字段与热加载 |
| [docs/troubleshooting/](docs/troubleshooting/) | 故障排查记录 |

新增文档类型时，同步更新本节和 [docs/OVERVIEW.md](docs/OVERVIEW.md)。

<p align="center">
  <a href="README.md"><img src="https://img.shields.io/badge/English-gray" alt="English"></a>
  <a href="README.zh-CN.md"><img src="https://img.shields.io/badge/%E7%AE%80%E4%BD%93%E4%B8%AD%E6%96%87-%E2%9C%93-blue" alt="简体中文"></a>
  <a href="README.ja.md"><img src="https://img.shields.io/badge/%E6%97%A5%E6%9C%AC%E8%AA%9E-gray" alt="日本語"></a>
  <a href="README.ko.md"><img src="https://img.shields.io/badge/%ED%95%9C%EA%B5%AD%EC%96%B4-gray" alt="한국어"></a>
</p>

<h1 align="center">subswap</h1>

<p align="center"><strong>工作和私人 Claude Code、ChatGPT、Codex、Cursor 账号，不用登出就能切换。</strong></p>

<p align="center">一台电脑上同时留着多个 AI 编程登录。一眼看出哪个账号还有用量，一条命令换过去——不用反复打开浏览器登录。</p>

<p align="center">
  <a href="https://github.com/x0c/subswap/actions/workflows/ci.yml"><img src="https://github.com/x0c/subswap/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/x0c/subswap/actions/workflows/release.yml"><img src="https://github.com/x0c/subswap/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/x0c/subswap" alt="License"></a>
</p>

<p align="center">
  <img src="docs/images/demo-swap.gif" width="920" alt="示例终端演示：列出账号后用 subswap swap 换到另一个 Codex 登录，无需登出">
</p>
<p align="center"><em>示例账号演示，不是你本机的真实录屏。</em></p>

<p align="center">
  <img src="docs/images/demo-status.svg" width="920" alt="账号列表与剩余额度示例，以及换号提示">
</p>

## 安装

在 **macOS 或 Linux，且已装 Homebrew** 时：

```bash
brew install x0c/tap/subswap
subswap
```

<details>
<summary>Windows / GitHub Release / 从源码安装</summary>

**Windows**

```powershell
irm https://raw.githubusercontent.com/x0c/subswap/main/install.ps1 | iex
```

安装器会下载最新 Windows Release、校验 SHA-256，并将 `subswap.exe` 加入当前用户的 `PATH`。也可以从[最新 Release](https://github.com/x0c/subswap/releases/latest)手动下载 zip 与校验和。

**GitHub Release（任意系统）**

从[最新 Release](https://github.com/x0c/subswap/releases/latest)下载，并在安装前校验随附 SHA-256 文件。

**从源码安装**（需要 Rust 1.80+，适合开发）

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --path crates/cli
subswap --help
```

日常使用优先选 Homebrew 或 Release 附件。

</details>

## 你能做什么

- **工作、个人和客户账号互不混淆** — 切换 Claude Code、ChatGPT、Codex、Cursor，不必反复登出再登录。
- **一眼看到可用额度** — Claude、Codex、Kimi、Cursor、OpenCode 与 Command Code 的额度窗口在同一列表。
- **网络不好也能手动切** — 手动 `swap` 不等待网络或额度接口；自动换号可选。
- **安全时才并行** — Claude、Codex、Kimi、OpenCode、Command Code 可在隔离环境跑，不改全局当前账号。
- **Cursor 桌面端** — 支持导入、切换与额度；不支持隔离的 `run` / `shell` / `env`。

## 第一次怎么用

先在对应客户端登录，然后：

```bash
subswap autoswap off   # 先试用手动切换
subswap                # 导入本机登录并列出账号
subswap swap 2         # 换成列表里的编号
```

<details>
<summary>更多导入 / 登录 / 隔离运行示例</summary>

```bash
subswap login kimi
subswap login cursor
subswap login opencode
subswap login commandcode
subswap login claude
subswap login codex

subswap swap alice@example.com
subswap swap claude/alice@example.com

subswap run codex bob@example.com -- --version
subswap shell claude/alice@example.com
eval "$(subswap env codex/bob@example.com)"
```

</details>

<details>
<summary>支持的客户端</summary>

| 客户端 | 导入与切换 | 额度与自动换号 | 隔离运行 | 重要边界 |
|---|---:|---:|---:|---|
| Claude Code | 是 | 是 | 是 | 自定义 API 端点只能手动选择。 |
| Codex CLI / ChatGPT | 是 | 是 | 是 | 额度查询走官方 app-server 通道。 |
| Kimi Code | 是 | 是 | 是 | 先在原生客户端登录，再导入。 |
| Cursor 桌面端 | 是 | 是 | 否 | 切换会协调桌面应用重启和 SQLite 状态。 |
| OpenCode Go | 是 | 是 | 是 | 只修改 `opencode-go` 项，其它项保持不变。 |
| Command Code | 是 | 是 | 是 | 切换 `~/.commandcode/auth.json`；额度走 `/alpha/billing/credits`。 |

CLI 已在 macOS、Linux、Windows CI 中测试。后台 daemon 仅支持 Unix：Linux 自动启动，macOS 需显式开启，Windows 仅使用前台 CLI。

</details>

<details>
<summary>环境自检（`subswap doctor`）</summary>

<p align="center">
  <img src="docs/images/demo-doctor.gif" width="920" alt="终端动图：subswap doctor 检查配置路径与各客户端凭证">
</p>

</details>

## 开始之前

- 只使用你拥有或被授权使用的账号。subswap 不共享凭证、不绕过服务限额，也不保证符合任何上游账号政策。
- Cursor 不能做隔离运行，因为它的身份是桌面应用状态；切换 Cursor 会协调关闭并重新打开应用。
- 在 Linux 上，第一次运行 `subswap` 会启动一个后台 daemon，用于额度检查和可选的自动换号。在 macOS 上需设置 `SUBSWAP_AUTO_DAEMON=1` 才开启。设置 `SUBSWAP_NO_DAEMON=1` 可完全关闭。
- 凭证数据保存在应用数据目录。macOS 与 Linux 会将私有凭证文件权限强制为 `0600`；Windows 依赖当前用户的应用数据权限。

## 安全保证

1. **手动切换在离线时仍可用。** 额度数据仅供参考：网络故障或令牌过期不会阻止 `subswap swap` 尝试完成本地切换。
2. **切换是事务性的。** 改变原生客户端状态前会先做私有快照，目标写入失败则回滚。
3. **自动换号有护栏。** 仅手动账号不会被自动选中；刚做完的手动选择有稳定窗口；未知或失败的额度数据按保守策略处理。
4. **原生客户端保留各自安全边界。** Codex 通过官方 app-server 刷新，Cursor 协调桌面生命周期，不支持的刷新状态会安全失败，而不是抢刷一次性令牌。

## 常见问题

### 手动换号会调用额度接口吗？

不会。`subswap swap` 是不依赖网络的逃生口。

### 凭证存在哪里？

私有凭证存在 subswap 应用数据目录，与账号元数据分开。Unix 上凭证与快照文件使用 `0600` 权限。自定义 Claude API 模式还需要把 API key 写在 Claude Code 设置里；切回 OAuth 时 subswap 会恢复受管设置。

### 能关掉自动换号吗？

可以。运行 `subswap autoswap off`，或用 `SUBSWAP_NO_DAEMON=1` 关闭后台 daemon。

### Cursor 和命令行客户端一样吗？

不完全一样。Cursor 支持导入、切换和额度，但不支持 `run` / `shell` / `env` 隔离，因为它的身份与桌面应用的 SQLite 状态绑定。

### 是不是只支持 Claude 或 Codex？

不是。目前支持 Claude Code、Codex / ChatGPT、Kimi Code、Cursor、OpenCode Go 和 Command Code。

## 贡献与安全

贡献路径与本地检查见 [CONTRIBUTING.md](CONTRIBUTING.md)。请勿在公开 Issue 中贴凭证、refresh token、登录文件、真实邮箱或账单截图。漏洞私下报告见 [SECURITY.md](SECURITY.md)。

如果它帮你少登出登录几次，请给仓库[点个 star](https://github.com/x0c/subswap)，方便以后再找到。

## 许可证

MIT — 见 [LICENSE](LICENSE)。

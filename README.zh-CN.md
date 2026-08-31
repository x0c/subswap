# subswap

[![CI](https://github.com/x0c/subswap/actions/workflows/ci.yml/badge.svg)](https://github.com/x0c/subswap/actions/workflows/ci.yml)
[![Release](https://github.com/x0c/subswap/actions/workflows/release.yml/badge.svg)](https://github.com/x0c/subswap/actions/workflows/release.yml)
[![License](https://img.shields.io/github/license/x0c/subswap)](LICENSE)

语言：[English](README.md) | 简体中文 | [日本語](README.ja.md) | [한국어](README.ko.md)

**一个尊重各客户端原生登录状态与额度边界的本地多账号切换工具。**

subswap 可以安全切换 Claude Code、OpenAI Codex / ChatGPT、Kimi Code、Cursor 和 OpenCode Go 账号。它把私有凭证快照留在本地，显示额度状态，并可在用量到达你的阈值时自动切到另一个符合条件的账号。

## 为什么用 subswap

- **工作、个人和客户账号互不混淆。** 无需反复登出、再登录。
- **一眼看到可用额度。** 在一个界面查看 Claude、Codex、Kimi、Cursor 与 OpenCode 的额度窗口。
- **始终由你决定。** 手动 `swap` 不等待网络或额度接口；自动换号可选，并会排除只允许手动选择的账号。
- **安全时才并行。** Claude、Codex、Kimi、OpenCode 能在隔离环境并行运行，不改变全局当前账号。

## 支持的客户端

| 客户端 | 导入与切换 | 额度与自动换号 | 隔离运行 | 重要边界 |
|---|---:|---:|---:|---|
| Claude Code | 是 | 是 | 是 | 自定义 API 端点只能手动选择。 |
| Codex CLI / ChatGPT | 是 | 是 | 是 | 额度查询走官方 app-server 通道。 |
| Kimi Code | 是 | 是 | 是 | 先在原生客户端登录，再导入。 |
| Cursor 桌面端 | 是 | 是 | 否 | 切换会协调桌面应用重启和 SQLite 状态。 |
| OpenCode Go | 是 | 是 | 是 | 只修改 `opencode-go` 项，其它项保持不变。 |

CLI 已在 macOS、Linux、Windows CI 中测试。后台 daemon 仅支持 Unix：Linux 自动启动，macOS 需显式开启，Windows 仅使用前台 CLI。

## 安装

### macOS / Linux

```bash
brew install x0c/tap/subswap
```

也可以从[最新 GitHub Release](https://github.com/x0c/subswap/releases/latest)下载，并在安装前校验随附 SHA-256 文件。

### Windows

```powershell
irm https://raw.githubusercontent.com/x0c/subswap/main/install.ps1 | iex
```

安装器会下载最新 Windows Release、校验 SHA-256，并将 `subswap.exe` 加入当前用户的 `PATH`。也可以从[最新 Release](https://github.com/x0c/subswap/releases/latest)手动下载 zip 与校验和。

### 从源码安装

适合开发或尝鲜未发布版本，要求 Rust 1.80+：

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --path crates/cli
subswap --help
```

`cargo install --git` 跟随仓库源码，并不等同于已验证的 Release；普通使用优先选择 Homebrew 或 Release 附件。

## 快速开始

### 导入已经在原生客户端登录的账号

```bash
# 导入当前本地登录状态、显示额度和当前账号。
subswap

# 需要时明确导入原生登录状态。
subswap login kimi
subswap login cursor
subswap login opencode

# 按账号 id 切换；重名时加客户端前缀。
subswap swap alice@example.com
subswap swap claude/alice@example.com
```

### 新增 Claude 或 Codex 账号

```bash
subswap login claude
subswap login codex
subswap
```

### 不改全局当前账号，直接运行一个账号

```bash
subswap run codex bob@example.com -- --version
subswap shell claude/alice@example.com
eval "$(subswap env codex/bob@example.com)"
```

## 使用前请了解

- 只管理你本人拥有或获授权使用的账号。subswap 不共享凭证、不绕过服务限制，也不保证任何用法符合上游服务条款。
- Cursor 的身份在桌面应用状态中，不能隔离运行；切换 Cursor 时会协调关闭并重新打开应用。
- Linux 上首次运行 `subswap` 会启动一个单实例后台 daemon，用于额度查询和可选自动换号。macOS 须设置 `SUBSWAP_AUTO_DAEMON=1` 才启用；设置 `SUBSWAP_NO_DAEMON=1` 可完全关闭。
- 凭证数据位于应用数据目录。macOS/Linux 的私有凭证文件强制为 `0600`；Windows 使用当前用户应用数据目录的系统权限。

## 安全保证

1. **手动切换不依赖网络。** 额度数据仅供参考；网络异常或 token 失效不会阻止 `subswap swap` 尝试本地切换。
2. **切换可回滚。** 修改原生客户端状态前会先写入私有快照；目标写入失败时会回滚。
3. **自动换号有护栏。** 只允许手动选择的账号永不自动选中；刚手动选择的账号会有宽限期；未知或失败的额度数据会被保守处理。
4. **遵守原生客户端边界。** Codex 通过官方 app-server 刷新，Cursor 协调桌面生命周期；无法安全刷新时宁可失败也不和一次性 token 竞争。

## 常见问题

### 手动换号会查询额度接口吗？

不会。`subswap swap` 是不依赖网络的逃生通道。

### 凭证保存在哪里？

私有凭证数据保存在 subswap 应用数据目录，和账号元数据分开。Unix 上凭证与快照文件使用 `0600` 权限。Claude 自定义 API 模式还需要把 API key 写入 Claude Code 设置；切回 OAuth 时 subswap 会恢复受管设置。

### 可以关闭自动换号吗？

可以。运行 `subswap autoswap off`，或设置 `SUBSWAP_NO_DAEMON=1` 关闭后台 daemon。

### Cursor 和命令行客户端一样吗？

不完全一样。Cursor 支持导入、切换与额度状态，但不支持 `run`、`shell`、`env` 隔离，因为它的身份与桌面应用 SQLite 状态协同管理。

### 它只支持 Claude 或 Codex 吗？

不是。当前支持 Claude Code、Codex / ChatGPT、Kimi Code、Cursor 和 OpenCode Go。

## 贡献与安全

贡献方式与本地检查见 [CONTRIBUTING.md](CONTRIBUTING.md)。不要在公开 issue 中贴凭证、refresh token、登录文件、真实邮箱或账单截图；私密漏洞报告见 [SECURITY.md](SECURITY.md)。

## License

MIT，见 [LICENSE](LICENSE)。

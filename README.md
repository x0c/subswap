<p align="center">
  <a href="README.md"><img src="https://img.shields.io/badge/English-%E2%9C%93-blue" alt="English"></a>
  <a href="README.zh-CN.md"><img src="https://img.shields.io/badge/%E7%AE%80%E4%BD%93%E4%B8%AD%E6%96%87-gray" alt="简体中文"></a>
  <a href="README.ja.md"><img src="https://img.shields.io/badge/%E6%97%A5%E6%9C%AC%E8%AA%9E-gray" alt="日本語"></a>
  <a href="README.ko.md"><img src="https://img.shields.io/badge/%ED%95%9C%EA%B5%AD%EC%96%B4-gray" alt="한국어"></a>
</p>

<h1 align="center">subswap</h1>

<p align="center"><strong>Check remaining quota and switch AI coding accounts.</strong></p>

<p align="center">Manage saved logins for Claude Code, Codex, Kimi Code, Cursor, OpenCode, and Command Code. See which account has quota left, then switch with one command.</p>

<p align="center">
  <a href="https://github.com/x0c/subswap/actions/workflows/ci.yml"><img src="https://github.com/x0c/subswap/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/x0c/subswap/actions/workflows/release.yml"><img src="https://github.com/x0c/subswap/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/x0c/subswap" alt="License"></a>
</p>

<p align="center">
  <img src="docs/images/demo-swap.gif" width="920" alt="Sample terminal demo: list accounts, then subswap swap to another Codex login without logging out">
</p>
<p align="center"><em>Demo with example accounts.</em></p>

## Install

On **macOS or Linux with Homebrew**:

Linux prebuilt packages require glibc 2.39 or newer; build from source on older systems.

```bash
brew install x0c/tap/subswap
subswap --help
```

<details>
<summary>Windows / prebuilt downloads / from source</summary>

**Windows**

```powershell
irm https://raw.githubusercontent.com/x0c/subswap/main/install.ps1 | iex
subswap --help
```

The installer downloads the latest Windows release, verifies its SHA-256 checksum, and adds `subswap.exe` to your user `PATH`. You can also download the zip and checksum from the [latest release](https://github.com/x0c/subswap/releases/latest).

**Prebuilt downloads**

The [latest release](https://github.com/x0c/subswap/releases/latest) includes macOS (Apple silicon / Intel), Linux (ARM64 / x64), and Windows (x64) packages. Verify the accompanying SHA-256 file before installing.

**From source** (current stable Rust, for development)

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --locked --path crates/cli
subswap --help
```

Prefer Homebrew or a release asset for normal use.

</details>

## First use

Install and sign in to a supported native client first. Automatic switching is enabled by default; start in manual mode to choose each account yourself:

```bash
subswap autoswap off
subswap                # save local logins and show accounts + quota
```

Save another account before switching between accounts. For Claude Code and Codex, subswap can start the native sign-in flow:

```bash
subswap login codex    # sign in to another Codex account; use claude for Claude Code
subswap swap 2         # replace 2 with a number shown in your account list
```

For Kimi Code, Cursor, and Command Code, sign in to the other account in the native client, then run `subswap login kimi`, `subswap login cursor`, or `subswap login commandcode` to save it. `subswap login opencode` imports official Console accounts and starts the official sign-in flow if needed; `subswap login opencode-api-key` imports Go API keys into a separate list.

After a Codex swap, restart an already-running Codex CLI session, or reload the IDE window and open a new session. Existing processes can retain the previous account. A Cursor desktop swap closes and reopens the app if it was running.

<details>
<summary>More commands</summary>

```bash
subswap swap alice@example.com
subswap swap claude/alice@example.com

subswap add-api         # add a Claude Code compatible API endpoint
subswap autoswap on     # enable automatic switching
subswap autoswap off    # return to manual mode
subswap doctor         # inspect local paths and client setup

subswap run codex bob@example.com -- --version
subswap shell claude/alice@example.com
eval "$(subswap env codex/bob@example.com)"
```

</details>

## What you can do

- **Switch saved accounts** — choose an account with `subswap swap <number>` instead of signing in again for every change.
- **Check quota and reset times** — see remaining usage across supported clients, including available Codex reset counts and expiration times when provided by the service.
- **Use custom Claude Code API endpoints** — add an Anthropic-compatible endpoint with `subswap add-api`; these accounts are selected manually.
- **Run another account in parallel** — use `run`, `shell`, or `env` for clients that support isolation, keeping the global active login unchanged.
- **Enable automatic switching** — let subswap select another saved account when the configured quota conditions are met.

## Supported clients

| Client / account type | Import and switch | Quota | Auto-swap | Isolated run | Notes |
|---|---:|---:|---:|---:|---|
| Claude Code (OAuth) | Yes | Yes | Yes | Yes | Custom API endpoints are manual-only. |
| Codex (ChatGPT login) | Yes | Yes | Yes | Yes | Restart existing Codex sessions after a global swap. |
| Kimi Code | Yes | Yes | Yes | Yes | Sign in with the native client, then import. |
| Cursor desktop / CLI | Yes | Yes | Yes | No | Desktop switching coordinates an app restart. |
| OpenCode Console | Yes | Yes | Yes | No | Automatic switching stays within official Console accounts. |
| OpenCode Go API key | Yes | Yes | No | V1 only | V2 key selection uses the official client. |
| Command Code | Yes | Yes | Yes | Yes | Sign in with the native client, then import. |

The CLI is tested in CI on macOS, Linux, and Windows. Install the native clients you want to use. Using the same account in multiple sessions can require signing in again if they refresh its credentials concurrently; use different saved accounts for parallel work where possible.

## Automatic switching

`subswap autoswap off` disables automatic switching for both the default command and the background daemon. `subswap autoswap on` enables it. Manual choices have a configurable grace period before automatic switching resumes.

Automatic decisions preserve the current account while its quota is loading, failed, or stale. Normally, a quota threshold triggers selection of a usable account. If the current account is confirmed exhausted and no confirmed usable target exists, subswap may select a confirmed-depleted account that recovers sooner. See [configuration](docs/CONFIG.md) for thresholds and timing.

The background daemon is Unix-only. Linux starts it automatically when `subswap` runs; macOS requires `SUBSWAP_AUTO_DAEMON=1`; Windows uses the foreground CLI. `SUBSWAP_NO_DAEMON=1` prevents auto-starting a daemon. It does not stop one already running or disable automatic decisions in the foreground command.

<details>
<summary>Environment check (`subswap doctor`)</summary>

<p align="center">
  <img src="docs/images/demo-doctor.gif" width="920" alt="Animated terminal demo: subswap doctor checks config paths and provider credentials">
</p>

</details>

## Credentials

Saved credentials are plaintext files in subswap's per-user application-data directory, separate from account metadata. On macOS and Linux, private credential and snapshot files use `0600` permissions; Windows relies on the current user's application-data permissions. `subswap doctor` shows the resolved paths. Native clients also keep their own active credentials. Custom Claude API mode writes its API key in Claude Code's settings; switching back to OAuth restores the managed settings from before API mode.

Manage only accounts you own or are authorized to use.

## FAQ

### Does a manual swap depend on quota APIs?

The account change does not depend on a quota lookup. After a successful swap, subswap prints a quota table, which may query the network when cached data is too old. A quota-query failure does not undo the completed account change. With no target, `subswap swap` only lists saved accounts and does not query quota.

### Does this change my ChatGPT browser login?

It manages the ChatGPT-backed login used by Codex. Your ChatGPT browser session is separate.

### Can I use Cursor with `run`, `shell`, or `env`?

Cursor supports import, switching, and quota status through its desktop or CLI credentials. It does not support these isolated-run commands.

## Contributing and security

Read [CONTRIBUTING.md](CONTRIBUTING.md) for the supported contribution paths and local checks. Do not open a public issue with credentials, refresh tokens, login files, real email addresses, or billing screenshots. See [SECURITY.md](SECURITY.md) for private vulnerability reporting.

If you find subswap useful, [star the repo](https://github.com/x0c/subswap) so you can find it again later.

## License

MIT — see [LICENSE](LICENSE).

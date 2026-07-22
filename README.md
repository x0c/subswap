# subswap - Claude, Codex, ChatGPT, Kimi and Cursor account switcher

[![CI](https://github.com/x0c/subswap/actions/workflows/ci.yml/badge.svg)](https://github.com/x0c/subswap/actions/workflows/ci.yml)
[![Release](https://github.com/x0c/subswap/actions/workflows/release.yml/badge.svg)](https://github.com/x0c/subswap/actions/workflows/release.yml)

Languages: English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | [한국어](README.ko.md)

subswap is a Rust CLI for managing multiple AI subscription accounts across
Claude Code, OpenAI Codex / ChatGPT, Kimi Code, and Cursor. It imports local
login state, stores private credential snapshots, checks quota windows, and
swaps the active account manually or automatically when usage crosses a
configurable threshold.

Use it as a Claude account switcher, Codex account manager, Kimi account
switcher, Cursor quota tracker, or a unified multi-provider subscription swapper.

**Platform support**: the CLI and all four providers support macOS, Linux, and Windows and are tested in CI.
The background daemon remains Unix-only; Windows uses the foreground CLI.

## Features

- **Multi-account swap for Claude Code, Codex CLI, Kimi Code, and Cursor**: flip the active account without re-logging-in.
- **Claude Code custom API endpoints**: add DeepSeek, Kimi, or another Anthropic-compatible endpoint through an interactive terminal wizard, then swap to and from it like any Claude account.
- **Account-isolated parallel environments for Claude, Codex, and Kimi**: `subswap run`, `shell`, and `env` project credentials into a private directory without touching the global active account. Cursor is intentionally excluded because its desktop SQLite state cannot be safely projected.
- **Quota-aware status**: view Claude/Kimi/Codex windows plus Cursor's `First-Party Models` and `API` percentages.
- **Automatic account swap**: a background daemon moves away from an account once usage crosses the configured threshold, and re-evaluates on every quota update to always pick the best available account.
- **Auto-swap toggle**: `subswap autoswap on/off` enables or disables automatic switching without touching the config file.
- **Settle-grace after manual swap**: after you manually pick an account, the daemon holds off for a grace period before auto-swapping away, so your intent isn't immediately overridden.
- **Network-independent manual swap**: `subswap swap` still works when quota APIs fail, tokens expire, or the network is down.
- **Quota result cache with stale fallback**: cached quota results are served while a fresh fetch is in flight, so the status screen is always responsive.
- **File-backed credential storage**: tokens are kept in an owner-only (`0600`) file under the app data directory, so reading quota never triggers OS keychain prompts. Credentials from older keyring-based installs are migrated automatically on first run.
- **Provider-based architecture**: Claude, Codex, Kimi, and Cursor are separate crates, so new AI providers can be added without changing core policy.

## Supported clients

| Provider | Local client | What subswap manages |
|---|---|---|
| Claude / Anthropic | Claude Code (`~/.claude`) | OAuth credentials, custom API endpoints, active account files, 5h / 7d quota, token keepalive |
| Codex / ChatGPT | Codex CLI (`~/.codex`) | `auth.json` passthrough, active account files, official app-server quota lookup |
| Kimi / Moonshot | Kimi Code (`~/.kimi-code`) | OAuth credential blob, active account file, 5h / 7d usage, coordinated token recovery |
| Cursor | Cursor desktop (`state.vscdb`) | account import/swap, `First-Party Models` and `API` usage, billing-cycle reset |

## Common use cases

- Switch between multiple Claude Pro, Claude Max, ChatGPT Plus, or ChatGPT Team seats.
- Keep a backup AI subscription ready when the current account reaches its usage limit.
- Run two accounts in separate terminals at the same time without interfering with each other.
- Check usage across accounts before starting a long coding session.
- Consolidate Claude, ChatGPT, Kimi, and Cursor account switching into one CLI.

## Status

| Milestone | Scope | State |
|---|---|---|
| M1 | workspace + core trait/model + minimal CLI | done |
| M2 | Claude provider: credential-backed swap, 5h/7d quota, best-effort token refresh | done |
| M3 | Codex provider: opaque auth.json passthrough, atomic write, official quota + compatible fallback | done |
| M4 | `subswapd` daemon: periodic poll + auto-swap + Claude token keepalive + zero-config auto-spawn | done |
| M5 | account-isolated run environments, auto-swap toggle, quota cache, settle-grace | done |
| M6 | Kimi and Cursor providers, official Codex quota channel, coordinated token recovery | done |

## Why

If you pay for more than one AI subscription, you probably hit one of:

- you ran out on Claude Pro and want to fall back to ChatGPT without re-logging-in;
- you keep two ChatGPT seats and want a one-liner to flip the active one;
- you want two accounts running in parallel in different terminals without conflict;
- you want to see how much of each window (5h / 7d) is left across accounts.

subswap stores each account's credentials in an owner-only file under its data directory (migrating any existing OS-keyring entries on first run), updates each native client's active state transactionally, and never blocks manual swap on quota lookup — quota data is advisory.

## Install

### macOS / Linux (Homebrew)

With Homebrew:

```bash
brew install x0c/tap/subswap
```

Or tap first, then install by name:

```bash
brew tap x0c/tap
brew install subswap
```

### Windows (PowerShell)

Install the latest release with one command:

```powershell
irm https://raw.githubusercontent.com/x0c/subswap/main/install.ps1 | iex
```

This installs `subswap.exe` and adds it to the current user's `PATH`. Windows does not include the
Unix-only `subswapd` daemon; run `subswap` whenever you want to refresh status or apply auto-swap.

### From source

From source, requires Rust 1.80+.

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --path crates/cli
subswap --help
```

You can also install directly from Git:

```bash
cargo install --git https://github.com/x0c/subswap --path crates/cli
```

## Quick start

```bash
# default: sync local active accounts, fetch quotas, auto-swap if past threshold,
# then print a one-screen status. Run this whenever you want to know what's up.
subswap

# manually swap to a specific account (escape hatch — never depends on the network)
subswap swap alice@example.com
# disambiguate when the same id exists under multiple providers:
subswap swap claude/alice@example.com

# interactively add DeepSeek, Kimi, or another Claude Code compatible API endpoint
subswap add-api
# custom API endpoints are manual-only and never participate in auto-swap
subswap swap deepseek

# Kimi and Cursor login commands import an account already signed in to the native client
subswap login kimi
subswap login cursor
# Cursor can then be selected like every other provider
subswap swap cursor/alice@example.com

# run an account in an isolated environment without touching the global active account
subswap run codex bob@example.com -- --version   # launch codex with bob's account
subswap shell alice@example.com                  # open an isolated sub-shell
eval "$(subswap env codex/bob@example.com)"      # export env vars into the current shell

# enable or disable automatic account switching
subswap autoswap on
subswap autoswap off

# remove an account from the registry and private credential store
subswap rm alice@example.com

# environment self-check (client files, keyring, config dirs)
subswap doctor
```

Accounts are picked up automatically from Claude Code, Codex CLI, Kimi Code, and Cursor local login state the first time you run `subswap`, as long as you have signed in to the corresponding native client once.

The first `subswap` invocation also spawns a detached background daemon on Unix platforms except macOS. The daemon polls quotas, auto-swaps in the background, and keeps Claude OAuth tokens fresh so a long-idle account doesn't 401 the moment you swap to it. macOS requires explicit opt-in because detached Keychain access can trigger extra authorization prompts: export `SUBSWAP_AUTO_DAEMON=1` to enable auto-start there. The daemon is single-instance (file-locked) and harmless to kill: `pkill -f 'subswap __daemon'` or `pkill subswapd`. To opt out entirely, export `SUBSWAP_NO_DAEMON=1`.

## Account-isolated environments

`subswap run / shell / env` let you use different Claude, Codex, or Kimi accounts in parallel across terminals without changing the global active account. Credentials are projected into a private directory and the native CLI is pointed there via environment variables (`CODEX_HOME`, `KIMI_CODE_HOME`, or Claude's config variables). Cursor is not supported here because its identity lives in the desktop app's SQLite state and requires a coordinated app restart.

```bash
subswap run codex 6 -- --version       # run codex as account #6 in isolation
subswap run claude alice@x.com         # run claude as alice in isolation
subswap shell 3                         # open a sub-shell isolated to account #3
eval "$(subswap env codex/bob@x.com)"  # temporarily point current shell at a codex account
```

- **Concurrency trade-off**: multiple isolated sessions may borrow the same account so global switching stays available; a rare simultaneous refresh may require one session to sign in again.
- **Global active warning**: starting an isolated session for the current global active account prints a warning, since a non-isolated client may be using it simultaneously and could invalidate the refresh token.

## Design invariants

These are load-bearing and worth knowing before contributing:

1. **`swap` never depends on quota lookups.** If the API is down, the keyring is unreachable, or the token is expired, manual swap must still flip the active account.
2. **Secrets stay out of registry metadata and snapshots are owner-only.** OAuth/API secrets live in the owner-only credential store. While a custom API is active, Claude Code also requires its API key in `~/.claude/settings.json`; subswap preserves and restores that file atomically.
3. **Swap is atomic and rollback-able.** Each `activate` writes a snapshot under `state_dir/snapshots/<ts>/` before touching anything; any failed write rolls back.
4. **Adding a provider = adding a `crates/providers/<id>` crate + registering it in `cli/src/app.rs::AppContext::build()`.** No provider-specific logic in `core`.
5. **Auto-swap threshold is centralized and configurable.** The compiled default lives in `crates/core/src/defaults.rs`, and runtime config can override it.

More: [`docs/`](docs/) (Chinese — internal collaboration docs).

## Comparison

| Tool | Focus | Difference |
|---|---|---|
| single-provider account switchers | one upstream at a time | subswap supports Claude, Codex / ChatGPT, Kimi, and Cursor behind one provider abstraction |
| quota dashboards | usage visibility only | subswap can also activate another local account when a quota window is full |
| manual login/logout | one account at a time | subswap keeps private account snapshots and swaps native client state transactionally |

## FAQ

### Does `subswap swap` call quota APIs?

No. Manual swap is an escape hatch and never depends on quota lookup. If the upstream API is down or a token is expired, `subswap swap claude/alice@example.com` still tries to activate that local account.

### Do custom API endpoints participate in auto-swap?

No. They are manual-only: subswap never automatically selects one, and auto-swap remains disabled while one is active. Manually swapping back to an OAuth account restores the Claude Code settings that existed before API mode.

### Where are tokens stored?

Tokens and refresh tokens live in owner-only credential files under the app data directory. While a custom API is active, Claude Code also requires its API key in `~/.claude/settings.json`; subswap keeps that file and switch snapshots owner-only.

### Why does Cursor briefly close during a swap?

Cursor may write its in-memory login state back to SQLite while exiting. subswap asks it to close first, commits the new account in one transaction, then reopens it. If writing or reopening fails, the old account state is restored.

### Does subswap race the native clients when refreshing tokens?

No refresh is attempted without a coordination boundary the native client understands. Codex refreshes only through the official app-server; Kimi uses the matching official lock protocol; Cursor active accounts are only re-read. Unsupported or old client versions safely keep the 401 instead of risking a one-time refresh token.

### Is this only for Claude or Codex?

No. Claude / Anthropic, Codex / ChatGPT, Kimi / Moonshot, and Cursor are supported today.

### Does it work on Windows?

Yes. The CLI and all four providers are tested on Windows in CI and released as a native zip; the PowerShell installer above handles installation and `PATH`. Only the background daemon is Unix-only.

## GitHub topics

Recommended repository topics after publishing:

`claude-code`, `codex-cli`, `chatgpt`, `kimi`, `moonshot-ai`, `cursor`, `anthropic`, `openai`, `account-switcher`, `quota-tracker`, `ai-tools`, `rust-cli`, `automation`

## Layout

```
crates/
  core/                # data model, Provider trait, CredentialStore, paths
  cli/                 # `subswap` binary
  daemon/              # `subswapd` binary
  providers/
    claude/            # Claude / Anthropic provider
    codex/             # Codex / ChatGPT provider
    kimi/              # Kimi / Moonshot provider
    cursor/            # Cursor desktop provider
```

## Contributing

Issues and PRs welcome. Notes:

- internal docs in `docs/` and `AGENTS.md` are in Chinese; code comments are in Chinese; everything user-visible (CLI text, error messages, tracing logs, crate descriptions) is in English.
- run `cargo check --workspace` and `cargo test --workspace` before opening a PR.

## License

MIT — see [`LICENSE`](LICENSE).

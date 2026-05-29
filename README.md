# subswap - Claude, Codex and ChatGPT account switcher

[![CI](https://github.com/x0c/subswap/actions/workflows/ci.yml/badge.svg)](https://github.com/x0c/subswap/actions/workflows/ci.yml)
[![Release](https://github.com/x0c/subswap/actions/workflows/release.yml/badge.svg)](https://github.com/x0c/subswap/actions/workflows/release.yml)

Languages: English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | [한국어](README.ko.md)

subswap is a Rust CLI for managing multiple AI subscription accounts across
Claude Code, Anthropic Claude, OpenAI Codex CLI, and ChatGPT. It imports local
login state, stores credentials in the OS keyring, checks quota windows, and
swaps the active account manually or automatically when usage crosses a
configurable threshold.

Use it as a Claude account switcher, Codex account manager, ChatGPT quota
tracker, or a unified multi-provider subscription swapper.

## Features

- **Multi-account swap for Claude Code and Codex CLI**: flip the active account without re-logging-in.
- **Quota-aware status**: view provider quota windows such as Claude 5h / 7d usage and Codex / ChatGPT usage data when available.
- **Automatic account swap**: a background daemon can move away from an account once usage crosses the configured threshold.
- **Network-independent manual swap**: `subswap swap` still works when quota APIs fail, tokens expire, or the network is down.
- **Keyring-backed credential storage**: secrets live in macOS Keychain, Windows Credential Manager, or Linux secret-service.
- **Provider-based architecture**: Claude / Anthropic and Codex / ChatGPT are separate crates, so new AI providers can be added without changing core policy.

## Supported clients

| Provider | Local client | What subswap manages |
|---|---|---|
| Claude / Anthropic | Claude Code (`~/.claude`) | OAuth credentials, active account files, 5h / 7d quota, token keepalive |
| Codex / ChatGPT | Codex CLI (`~/.codex`) | `auth.json` passthrough, active account files, ChatGPT usage lookup |

## Common use cases

- Switch between multiple Claude Pro, Claude Max, ChatGPT Plus, or ChatGPT Team seats.
- Keep a backup AI subscription ready when the current account reaches its usage limit.
- Check usage across accounts before starting a long coding session.
- Consolidate Claude and ChatGPT account switching into one CLI.

## Status

| Milestone | Scope | State |
|---|---|---|
| M1 | workspace + core trait/model + minimal CLI | done |
| M2 | Claude provider: keyring-backed swap, 5h/7d quota, best-effort token refresh | done |
| M3 | Codex provider: opaque auth.json passthrough, atomic write, tolerant wham/usage parsing | done |
| M4 | `subswapd` daemon: periodic poll + auto-swap + Claude token keepalive + zero-config auto-spawn | done |

## Why

If you pay for more than one AI subscription, you probably hit one of:

- you ran out on Claude Pro and want to fall back to ChatGPT without re-logging-in;
- you keep two ChatGPT seats and want a one-liner to flip the active one;
- you want to see how much of each window (5h / 7d) is left across accounts.

subswap stores each account in the OS keyring (Keychain / Credential Manager / secret-service), swaps the active one atomically across all clients that read the same on-disk credential file, and never blocks swap on the network — quota lookups are advisory.

## Install

With Homebrew:

```bash
brew install x0c/tap/subswap
```

Or tap first, then install by name:

```bash
brew tap x0c/tap
brew install subswap
```

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

# remove an account from the registry and the keyring
subswap rm alice@example.com

# environment self-check (client files, keyring, config dirs)
subswap doctor
```

Accounts are picked up automatically from `~/.claude` and `~/.codex` the first time you run `subswap`, as long as you have logged into Claude Code / Codex CLI at least once.

The first `subswap` invocation also spawns a detached background daemon which polls quotas and auto-swaps in the background, and keeps Claude OAuth tokens fresh so a long-idle account doesn't 401 the moment you swap to it. The daemon is single-instance (file-locked), Unix-only, and harmless to kill: `pkill -f 'subswap __daemon'` or `pkill subswapd`. To opt out entirely, export `SUBSWAP_NO_DAEMON=1`.

## Design invariants

These are load-bearing and worth knowing before contributing:

1. **`swap` never depends on quota lookups.** If the API is down, the keyring is unreachable, or the token is expired, manual swap must still flip the active account.
2. **Secrets only live in the OS keyring.** `registry.toml`, the audit log, and snapshots never contain plaintext tokens or refresh tokens.
3. **Swap is atomic and rollback-able.** Each `activate` writes a snapshot under `state_dir/snapshots/<ts>/` before touching anything; any failed write rolls back.
4. **Adding a provider = adding a `crates/providers/<id>` crate + one line in `cli/src/main.rs::AppContext::build()`.** No provider-specific logic in `core`.
5. **Auto-swap threshold is centralized and configurable.** The compiled default lives in `crates/core/src/defaults.rs`, and runtime config can override it.

More: [`docs/`](docs/) (Chinese — internal collaboration docs).

## Comparison

| Tool | Focus | Difference |
|---|---|---|
| single-provider account switchers | one upstream at a time | subswap supports Claude and Codex / ChatGPT behind one provider abstraction |
| quota dashboards | usage visibility only | subswap can also activate another local account when a quota window is full |
| manual login/logout | one account at a time | subswap keeps registered accounts in the keyring and swaps active local files atomically |

## FAQ

### Does `subswap swap` call quota APIs?

No. Manual swap is an escape hatch and never depends on quota lookup. If the upstream API is down or a token is expired, `subswap swap claude/alice@example.com` still tries to activate that local account.

### Where are tokens stored?

Tokens and refresh tokens are stored only in the OS keyring. `registry.toml`, audit logs, and snapshots are designed not to contain plaintext secrets.

### Is this only for Claude?

No. The first supported providers are Claude / Anthropic and Codex / ChatGPT. The core crate exposes a Provider trait so future AI subscription providers can be added as separate crates.

## GitHub topics

Recommended repository topics after publishing:

`claude-code`, `codex-cli`, `chatgpt`, `anthropic`, `openai`, `account-switcher`, `quota-tracker`, `ai-tools`, `rust-cli`, `keyring`, `automation`

## Layout

```
crates/
  core/                # data model, Provider trait, CredentialStore, paths
  cli/                 # `subswap` binary
  daemon/              # `subswapd` binary
  providers/
    claude/            # Claude / Anthropic provider
    codex/             # Codex / ChatGPT provider
```

## Contributing

Issues and PRs welcome. Notes:

- internal docs in `docs/` and `AGENTS.md` are in Chinese; code comments are in Chinese; everything user-visible (CLI text, error messages, tracing logs, crate descriptions) is in English.
- run `cargo check --workspace` and `cargo test --workspace` before opening a PR.

## License

MIT — see [`LICENSE`](LICENSE).

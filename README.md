# subswap

One CLI to manage multiple AI-subscription accounts (Claude / Codex), check quotas, and swap the active one — manually or automatically when the current one crosses the usage threshold.

> Inspired by [Loongphy/codex-auth](https://github.com/Loongphy/codex-auth) and [realiti4/claude-swap](https://github.com/realiti4/claude-swap). subswap merges both behind a Provider abstraction and adds threshold-based auto-swap.

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

Requires Rust 1.80+.

```bash
git clone https://github.com/<you>/subswap
cd subswap
cargo install --path crates/cli
subswap --help
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

The first `subswap` invocation also spawns a detached background daemon (`subswapd`) which polls quotas and auto-swaps in the background, and keeps Claude OAuth tokens fresh so a long-idle account doesn't 401 the moment you swap to it. The daemon is single-instance (file-locked), Unix-only, and harmless to kill: `pkill subswapd`. To opt out entirely, export `SUBSWAP_NO_DAEMON=1`.

## Design invariants

These are load-bearing and worth knowing before contributing:

1. **`swap` never depends on quota lookups.** If the API is down, the keyring is unreachable, or the token is expired, manual swap must still flip the active account.
2. **Secrets only live in the OS keyring.** `registry.toml`, the audit log, and snapshots never contain plaintext tokens or refresh tokens.
3. **Swap is atomic and rollback-able.** Each `activate` writes a snapshot under `state_dir/snapshots/<ts>/` before touching anything; any failed write rolls back.
4. **Adding a provider = adding a `crates/providers/<id>` crate + one line in `cli/src/main.rs::AppContext::build()`.** No provider-specific logic in `core`.
5. **Auto-swap threshold defaults to 0.99.** When `used / limit >= 99%`, the current account is considered exhausted.

More: [`docs/`](docs/) (Chinese — internal collaboration docs).

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

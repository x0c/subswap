# Contributing to subswap

Thanks for considering a contribution. subswap is a small Rust workspace, and the project tries to stay small. Read this before opening a PR — most rejections are about scope.

## Project model

- **Cargo workspace** rooted at `Cargo.toml`. Members:
  - `crates/core` — data model, `Provider` trait, credential store, paths, audit log, auto-swap policy.
  - `crates/cli` — `subswap` binary (the user-facing CLI).
  - `crates/daemon` — `subswapd` binary (background poller, auto-spawned by the CLI).
  - `crates/providers/<id>` — one crate per upstream (`claude`, `codex`).
- Internal collaboration docs live in `docs/` and are written in **Chinese**. Code comments are in **Chinese**. Everything user-visible (CLI output, error messages, tracing logs, Cargo descriptions) is in **English**.

## Before you start

1. Read [`AGENTS.md`](AGENTS.md) — it codifies the load-bearing invariants. The ones most likely to bite you:
   - Manual `swap` must never depend on the network or quota lookups.
   - Secrets only live in the OS keyring; `registry.toml`, the audit log, and snapshots must never contain plaintext tokens or refresh tokens.
   - `async fn` must not do blocking IO directly — wrap `fs2`, `std::fs`, `keyring` calls in `tokio::task::spawn_blocking`.
   - Don't poll high-frequency to "probe" rate limits.
2. Skim [`docs/OVERVIEW.md`](docs/OVERVIEW.md) — index for the rest of the docs.
3. If you're adding a new provider, read [`docs/PROVIDER_KNOWLEDGE_BASE.md`](docs/PROVIDER_KNOWLEDGE_BASE.md) and [`docs/design/ARCHITECTURE.md`](docs/design/ARCHITECTURE.md) first.

## Local checks

Run these locally before opening a PR — CI runs the same ones and will reject on diff.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Quick sanity for the CLI surface:

```bash
cargo run -p subswap-cli -- --help
cargo run -p subswap-cli -- doctor
```

When iterating on the daemon, kill any stale background instance first:

```bash
pkill subswapd 2>/dev/null
```

## Adding a new provider

1. Create `crates/providers/<id>/` mirroring `claude` / `codex`. Implement `subswap_core::Provider`.
2. Register the crate in the root `Cargo.toml` `members` list and `[workspace.dependencies]`.
3. Wire it into `crates/cli/src/main.rs::AppContext::build()` and `sync_local_active()`.
4. Wire it into `crates/daemon/src/main.rs::main()` if it benefits from periodic polling.
5. Document the upstream endpoints, local files, and quirks in `docs/PROVIDER_KNOWLEDGE_BASE.md`.
6. Add unit tests in the provider crate. Integration tests should use a mock HTTP server, not real upstream.

## Style

- **Code comments in Chinese.** Don't translate existing Chinese comments; do match the style when writing new ones.
- **User-visible strings in English, terse.** Success path one short line (e.g. `swap → claude/alice`). Verbose hints only on failure.
- **Document the "why", not the "what".** Names should carry "what".
- Run `rustfmt`; the CI gate is strict.

## Commit messages and PRs

- Subject line: short, imperative ("add codex rate-limit cache" rather than "added").
- Body: explain the *why* and any non-obvious tradeoffs. Don't paste the diff.
- One logical change per PR. If a refactor is bundled with a behavior change, split the PR.
- Reference issue numbers if applicable.

## Scope guidance

Out of scope without prior discussion:

- New external dependencies (especially those pulling C libraries).
- New top-level CLI subcommands. The surface is intentionally tiny.
- Provider-specific logic in `crates/core`. Keep it abstract.
- Anything that bypasses upstream rate limits, terms of service, or scrapes opaque endpoints aggressively.

In scope:

- Bug fixes (with a regression test).
- New providers behind the existing `Provider` trait.
- Better error messages, doctor checks, and tracing context.
- Docs improvements.

## Reporting bugs

Open a GitHub issue with:

1. Output of `subswap doctor`.
2. `subswap --log debug` for the failing invocation (redact tokens).
3. OS + Rust version (`rustc --version`).
4. Whether the issue reproduces with `SUBSWAP_NO_DAEMON=1`.

## License

By contributing, you agree your contributions are licensed under the MIT license (see [`LICENSE`](LICENSE)).

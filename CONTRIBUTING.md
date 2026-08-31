# Contributing to subswap

Thank you for improving subswap. It manages local credentials and native client state, so correctness and scope matter more than feature count.

## Before you start

- Read [AGENTS.md](AGENTS.md). It contains the project's non-negotiable safety, release, and verification rules.
- Never commit credentials, refresh tokens, API keys, complete login files, real email addresses, or billing screenshots.
- Do not add behavior intended to share credentials, bypass limits, evade provider policies, or aggressively probe quota endpoints.
- User-visible CLI text, errors, and logs are English. Internal collaboration docs and code comments are Chinese.

## Project shape

subswap is a Rust workspace with a small core, a CLI, a background daemon, and one adapter per native client:

- Claude Code, Codex / ChatGPT, Kimi Code, Cursor, and OpenCode Go are supported.
- File-based OAuth clients share the common switching engine where their safety boundary permits it.
- Claude and Cursor keep dedicated adapters because their credential storage, API mode, desktop lifecycle, and refresh coordination are different.

When adding or changing a provider, read [the provider knowledge base](docs/PROVIDER_KNOWLEDGE_BASE.md) and [the architecture guide](docs/design/ARCHITECTURE.md) first. Do not force a provider into the shared engine when it needs a different native-client safety boundary.

## Local checks

Run these before opening a pull request:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace
```

For a CLI-facing change, also run `subswap --help` or the affected command in the project's isolated test environment. Tests must never touch a real login keychain or local credential file.

## Pull request expectations

- Keep one user-visible behavior change per pull request.
- Add a regression test for every bug fix.
- Explain user impact, safety trade-offs, and verification in the pull request description.
- Update the public README, supported-client matrix, translations, and release notes whenever a provider, platform, installation path, or user-visible boundary changes.
- Do not add a new dependency or top-level command without first explaining why the existing surface cannot cover the need.

## Reporting bugs

Use the bug-report form and include a redacted `subswap doctor` result, operating system, native client version, expected and actual behavior, and whether the background daemon was enabled. Never paste secrets or complete native login files into a public issue.

## Security issues

Do not report a vulnerability in a public issue. Follow [SECURITY.md](SECURITY.md) instead.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](LICENSE).

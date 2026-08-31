# subswap

[![CI](https://github.com/x0c/subswap/actions/workflows/ci.yml/badge.svg)](https://github.com/x0c/subswap/actions/workflows/ci.yml)
[![Release](https://github.com/x0c/subswap/actions/workflows/release.yml/badge.svg)](https://github.com/x0c/subswap/actions/workflows/release.yml)
[![License](https://img.shields.io/github/license/x0c/subswap)](LICENSE)

Languages: English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | [한국어](README.ko.md)

**A local-first multi-account switcher for AI coding tools that respects each client's native login state and quota boundaries.**

subswap safely switches accounts for Claude Code, OpenAI Codex / ChatGPT, Kimi Code, Cursor, and OpenCode Go. It keeps private local credential snapshots, shows quota status, and can optionally move to another eligible account when usage reaches your threshold.

## Why subswap

- **Keep work, personal, and client accounts separate.** Switch an account without repeatedly logging out and back in.
- **Know your remaining headroom.** See Claude, Codex, Kimi, Cursor, and OpenCode quota windows in one place.
- **Stay in control.** A manual `swap` never waits for a network or quota API; automatic swapping is optional and respects accounts marked manual-only.
- **Use parallel terminals when it is safe.** Claude, Codex, Kimi, and OpenCode can run in isolated environments without changing the global active account.

## Supported clients

| Client | Import and switch | Quota and auto-swap | Isolated run | Important boundary |
|---|---:|---:|---:|---|
| Claude Code | Yes | Yes | Yes | Custom API endpoints are manual-only. |
| Codex CLI / ChatGPT | Yes | Yes | Yes | Quota lookup uses the official app-server channel. |
| Kimi Code | Yes | Yes | Yes | Sign in with the native client, then import. |
| Cursor desktop | Yes | Yes | No | Switching coordinates a desktop-app restart and its SQLite state. |
| OpenCode Go | Yes | Yes | Yes | Only the `opencode-go` entry is changed; other entries stay untouched. |

The CLI is tested in CI on macOS, Linux, and Windows. The background daemon is Unix-only: it auto-starts on Linux, requires explicit opt-in on macOS, and is unavailable on Windows.

## Install

### macOS / Linux

```bash
brew install x0c/tap/subswap
```

Or use the [latest GitHub Release](https://github.com/x0c/subswap/releases/latest) and verify the accompanying SHA-256 file before installing.

### Windows

```powershell
irm https://raw.githubusercontent.com/x0c/subswap/main/install.ps1 | iex
```

The installer downloads the latest Windows release, verifies its SHA-256 checksum, and adds `subswap.exe` to your user `PATH`. You can also download the zip and checksum yourself from the [latest release](https://github.com/x0c/subswap/releases/latest).

### From source

For development or an unreleased build, Rust 1.80+ is required:

```bash
git clone https://github.com/x0c/subswap
cd subswap
cargo install --path crates/cli
subswap --help
```

`cargo install --git` follows repository source rather than a verified release. Prefer Homebrew or a release asset for normal use.

## Quick start

### Import an account already signed in to a native client

```bash
# Import the current local login state, show quota status, and print the active account.
subswap

# Import a native login explicitly when needed.
subswap login kimi
subswap login cursor
subswap login opencode

# Switch by account id, or add the client prefix when the id is ambiguous.
subswap swap alice@example.com
subswap swap claude/alice@example.com
```

### Add another Claude or Codex account

```bash
subswap login claude
subswap login codex
subswap
```

### Run an account without changing the global active account

```bash
subswap run codex bob@example.com -- --version
subswap shell claude/alice@example.com
eval "$(subswap env codex/bob@example.com)"
```

## Before you start

- Use only accounts that you own or are authorized to use. subswap does not share credentials, bypass service limits, or make any upstream account policy compliant.
- Cursor cannot be used in an isolated run because its identity is desktop-app state; a Cursor swap coordinates closing and reopening the app.
- On Linux, the first `subswap` run starts a single background daemon for quota checks and optional auto-swap. On macOS, set `SUBSWAP_AUTO_DAEMON=1` to opt in. Set `SUBSWAP_NO_DAEMON=1` to disable it entirely.
- Credential data stays in the application data directory. On macOS and Linux, private credential files are forced to `0600`; Windows relies on the current user's application-data permissions.

## Safety guarantees

1. **Manual switching stays available offline.** Quota data is advisory: network trouble or an expired token does not stop `subswap swap` from attempting the local switch.
2. **Switches are transactional.** subswap takes a private snapshot before changing native client state and rolls back if a target write fails.
3. **Automatic switching has guardrails.** Manual-only accounts are never selected automatically; a settle period preserves a just-made manual choice; unknown or failed quota data is handled conservatively.
4. **Native clients keep their own safety boundary.** Codex refreshes through its official app-server, Cursor coordinates its desktop lifecycle, and unsupported refresh states fail safely instead of racing a one-time token.

## FAQ

### Does a manual swap call quota APIs?

No. `subswap swap` is the network-independent escape hatch.

### Where are credentials stored?

Private credential data is stored in the subswap application-data directory, separate from account metadata. On Unix, credential and snapshot files use `0600` permissions. Custom Claude API mode also needs its API key in Claude Code's settings; subswap restores the managed settings when you switch back to OAuth.

### Can I turn off automatic switching?

Yes. Run `subswap autoswap off`, or disable the background daemon with `SUBSWAP_NO_DAEMON=1`.

### Does Cursor work like the command-line clients?

Not completely. Cursor supports import, switching, and quota status, but not `run`, `shell`, or `env` isolation because its identity is coordinated with the desktop application's SQLite state.

### Is this only for Claude or Codex?

No. Claude Code, Codex / ChatGPT, Kimi Code, Cursor, and OpenCode Go are supported today.

## Contributing and security

Read [CONTRIBUTING.md](CONTRIBUTING.md) for the supported contribution paths and local checks. Do not open a public issue with credentials, refresh tokens, login files, real email addresses, or billing screenshots. See [SECURITY.md](SECURITY.md) for private vulnerability reporting.

## License

MIT — see [LICENSE](LICENSE).

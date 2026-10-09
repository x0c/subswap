# Active Codex quota stays loading while parked accounts return quickly

## Verified cause

Subswap 1.14.5 sends newline-delimited JSON through `codex app-server proxy`, which relays raw bytes. Codex 0.161.0's Unix control socket requires a WebSocket upgrade. The TCP-independent socket connection succeeds, but initialization never reaches the RPC server: subswap waits eight seconds before starting a fallback process. A real default-entry query took 20.7 seconds; the active Codex row also waited about five seconds for optional reset-credit details after obtaining its main quotas.

A read-only WebSocket probe initialized in 2 ms and returned rate limits in 1.73 seconds. That response belonged to a different account from live `auth.json`. Transport speed does not establish account ownership; an already running Codex daemon can retain the previous login after a swap.

Sources: [Codex 0.161.0 Unix WebSocket transport](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/app-server-transport/src/transport/unix_socket.rs), [raw stdio proxy](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/stdio-to-uds/src/lib.rs), [official rate-limit/reset-credit response](https://learn.chatgpt.com/docs/app-server).

## Required behavior

- Connect to an existing Codex Unix control socket using its WebSocket transport and the official initialization handshake. Do not pay a predictable eight-second protocol mismatch on each query.
- Verify account ownership before requesting quotas when the server exposes account identity, and always verify the quota response's `accountId` before using or caching it. A stale server must fall back without refreshing the wrong account or restarting the user's Codex sessions.
- Preserve active-account refresh coordination and the rule that 429/service errors never trigger another quota request through a fallback channel.
- Reuse reset-credit details already included in the official response. Optional detail requests must have a small bounded budget and must never turn successfully fetched main quotas into an overall timeout. Keep the authoritative count even when detail rows are incomplete or capped.
- Active queries must not make a separate reset-detail HTTP request: show the authoritative count immediately when inline details are unavailable. Parked detail lookups have a configurable one-second budget (`codex.reset_details_timeout_ms`); an unavailable expiration does not hide the reset count or main quotas.
- Continue respecting the shared quota cache and conservative request cadence. No additional high-frequency polling.

## Acceptance

Regression coverage must exercise a real local WebSocket server, stale-account rejection, authentication and 429 boundaries, reset-credit detail reuse and incomplete-detail handling. Run workspace formatting, compiler/lint checks, tests, debug/release builds, then inspect timed default-entry output from the installed committed release. Report the measured active-account completion time separately from unrelated providers.

## Verification (1.14.6)

On macOS with Rust 1.95.0 and Codex 0.161.0: workspace formatting, all-target compiler check, all-target clippy with warnings denied, all-target tests (352 passed, zero ignored), workspace debug build, and locked CLI/daemon release build passed. Complete diagnostic logs contained no compiler or lint warnings. New WebSocket dependencies declare Rust 1.63 support (the project baseline is 1.80); Linux and Windows acceptance is handled by the release CI matrix.

A real interactive default-entry query from the debug build completed the active Codex row in 1.521 seconds and the whole command in 2.324 seconds. The shared cache timestamp confirms a fresh sample, and there was no control-channel error or compatible fallback. Five-hour/weekly quotas and the reset expiration were displayed. This is a single live latency sample, not a network-independent guarantee. Logs are stored under `~/.config/subswap/verification/20261009-active-quota/`.

# Active Claude quota shows `credentials expired; open native client` while Claude Code still works

## Symptom and verified evidence

Observed 2026-10-25 with installed `subswap 1.15.1` and Claude Code 2.1.310. The owner reported that this had been happening for a long time, including after one or two days without opening Claude Code, while Claude Code itself kept working.

- At 10:46 local the default list showed the active subscription account as `credentials expired; open native client`.
- The account's poll record (`<cache_dir>/claude-usage/<sha256(email)>.json`) had 17 consecutive `Claude credential expired; open Claude Code to renew` failures, a retry deadline at 11:01, and a last success the previous day.
- The live Keychain item `Claude Code-credentials` had a valid refresh token. Its access token was issued at 10:47:03, when a Claude Code session started and renewed it. Claude Code access tokens last about 8 hours.

## Cause

1. In 1.15.1, active credentials were strictly read-only ([active quota native null](2026-10-11-claude-active-quota-native-null.md)). Any time Claude Code went unused for more than the access-token lifetime, the active quota could not be read until Claude Code ran again.
2. The local "expired" precondition failure went through the same escalating per-account backoff as upstream failures. After Claude Code renewed the token, subswap kept showing the old error until the backoff deadline.

## Fix (1.16.0)

Requirement and protocol: [PROVIDER_KNOWLEDGE_BASE — Active credential renewal under the official lock](../PROVIDER_KNOWLEDGE_BASE.md#active-credential-renewal-under-the-official-lock-2026-10-25).

- An expired active credential, or a usage 401 on the live token, is renewed only while holding Claude Code's official refresh locks (`<dir>/.oauth_refresh.lock` plus the legacy `<realpath(dir)>.lock`). The client must report at least the verified version (`claude --version` ≥ 2.1.310). Under the lock subswap re-reads live, adopts a sibling's renewal, refreshes once otherwise, and saves with compare-and-swap through `/usr/bin/security`.
- The lock, refresh POST and write run on a blocking thread with its own runtime, so quota timeouts cannot drop a rotated token. A lock holder that loses its lock never touches or deletes the replacement owner's directory. Future-dated locks are never reaped.
- The per-account poll record stores a fingerprint of the access token behind a failure. A non-429 failure does not block a query made with a different token. 429 deadlines still apply per account.

## Verification (2026-10-25)

- macOS ARM64: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` (0 warnings), `cargo test --workspace` (466 passed). New unit tests cover single refresh plus field-preserving write, sibling adoption, primary/legacy lock contention without refresh, `invalid_grant` dead-token guard (second attempt sends nothing, live never cleared), account change abort, compare-and-swap adoption of a newer write, per-credential backoff, stale versus future-dated lock reaping, and replacement-lock ownership. The CLI surface test confirms that a client older than the verified version stays at zero refresh with an untouched isolated keychain.
- Real machine (temporary ignored probe, removed after running): against the real `~/.claude` locks and Keychain, the renewal path acquired both official locks, adopted the current live token without a refresh request, left the Keychain byte-identical and released both locks (57 ms).
- The dev build of the default list showed the active account's 5h/7d readings once the old deadline passed.
- Not yet observed: a natural expiry renewal on the real account. Forcing a rotation while several Claude Code processes were running was deliberately not done. Check the first default-list run after more than 8 hours of Claude Code idleness.

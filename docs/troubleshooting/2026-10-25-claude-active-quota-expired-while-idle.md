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

### Linux lock ownership correction (1.16.3)

Release CI for 1.16.1 failed `proper_lock::tests::reaped_holder_never_touches_or_deletes_replacement_lock` on Ubuntu. The old ownership check used only `(dev, ino)`. After a lock directory was removed, Linux could allocate the same inode number to a same-named replacement; the old holder then treated the replacement as its own, refreshed its mtime and could delete it on drop. macOS did not reproduce this reliably.

`ProperLock` now keeps the directory handle open for its whole lifetime, so the original inode stays allocated and cannot be reused by a replacement. Ownership requires both the held handle's identity and the current path's identity to match; heartbeat and `Drop` use the same proof. On Windows the held handle is opened with `FILE_SHARE_READ | WRITE | DELETE` and `FILE_FLAG_BACKUP_SEMANTICS`, and identity comes from `GetFileInformationByHandle`, so the official client's rmdir/rename protocol still works. A real replacement lock surviving the old holder's heartbeat and drop is covered by `replacement_lock_survives_old_holder_heartbeat_and_drop`.

## Verification (2026-10-25)

- macOS ARM64: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings` (0 warnings), `cargo test --workspace` (468 passed). New unit tests cover single refresh plus field-preserving write, sibling adoption, primary/legacy lock contention without refresh, `invalid_grant` dead-token guard (second attempt sends nothing, live never cleared), account change abort, compare-and-swap adoption of a newer write, per-credential backoff, stale versus future-dated lock reaping, and replacement-lock ownership. The CLI surface test confirms that a client older than the verified version stays at zero refresh with an untouched isolated keychain.
- Linux: the inode-reuse regression test and the replacement-lock test pass on Ubuntu 22.04. The v1.16.3 release CI validated the same tests on Ubuntu, macOS and Windows; all five build targets and the Windows installer verification passed.
- Real machine (temporary ignored probe, removed after running): against the real `~/.claude` locks and Keychain, the renewal path acquired both official locks, adopted the current live token without a refresh request, left the Keychain byte-identical and released both locks (57 ms).
- The dev build of the default list showed the active account's 5h/7d readings once the old deadline passed.
- Not yet observed: a natural expiry renewal on the real account. Forcing a rotation while several Claude Code processes were running was deliberately not done. Check the first default-list run after more than 8 hours of Claude Code idleness.

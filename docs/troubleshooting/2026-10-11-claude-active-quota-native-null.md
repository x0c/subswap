# Active Claude account shows `usage unavailable` after the native-query path

## Symptom and verified evidence

Observed 2026-10-11 with installed `subswap 1.15.0` and Claude Code 2.1.296.

- The default list showed the active Claude subscription account as `quota usage unavailable`; the other account showed `needs re-login` (the account whose login was lost on 2026-10-09, see [persistent Claude usage 429](2026-10-09-claude-persistent-usage-429.md)).
- The active account's per-account poll record (`<cache_dir>/claude-usage/<sha256(email)>.json`) held `Claude usage unavailable; native client returned no limits`, two failures and a 30-minute retry deadline. Its last successful reading was taken the previous day while the account was **parked**, through the direct OAuth query.
- The active credential was still valid (declared expiry hours in the future). The account had been made active by a manual swap shortly before the failures; from then on, 1.15.0 routed it through the official client.
- Claude Code had no `cachedUsageUtilization` snapshot in its global configuration, so the zero-request snapshot source was empty.

## Cause

1.15.0 queried the active account by starting `claude -p` and sending the experimental `get_usage` control request. Inspection of the installed 2.1.296 bundle established:

- `get_usage` calls the same `/api/oauth/usage` service. When that fetch is not `ok` (HTTP 429, an in-band `rate_limit_error`, or another failure) and the short-lived process has no rate-limit headers or persisted snapshot to seed from, it answers `rate_limits_available: true, rate_limits: null`. The failure reason (`rateLimitedVia`) is **not** included in the control response. subswap could therefore only report "no limits", never "429" or "auth failed".
- The native usage fetch runs with `refreshOAuth: true`. Starting the official client to read quota can rotate the shared one-time refresh token, which matches the 2026-10-09 login loss. The native path was not side-effect-free and did not bypass usage throttling.
- Rate-limit headers from normal model replies stay in the running session's memory; Claude Code persists `cachedUsageUtilization` only after a successful usage fetch.

The exact upstream response for the two native failures was not captured, so 429 versus another failure for this account remains unmeasured.

## External findings (2026-10-11)

- Persistent usage 429 is a per-account bucket and continues to be reported ([#30930](https://github.com/anthropics/claude-code/issues/30930), [#31637](https://github.com/anthropics/claude-code/issues/31637)). Mature monitors converge on one cross-process escalating backoff that honors `Retry-After` and keeps 429 terminal for the attempt ([Win-CodexBar #775](https://github.com/nesszer/Win-CodexBar/issues/775), [#785](https://github.com/nesszer/Win-CodexBar/pull/785)).
- Rotating the token does not reopen the bucket and risks invalidating the refresh token held by Claude Code ([usagebar #4](https://github.com/ghandhitechnology/usagebar/pull/4)).
- The claim that `User-Agent: claude-code/<version>` gets a more generous bucket is anecdotal, without measurements ([usage-pill #1](https://github.com/kcao-gss/usage-pill/issues/1)); it is not adopted.
- One-token model probes reading `anthropic-ratelimit-unified-*` headers consume quota; not adopted.

## Fix (1.15.1)

- Active subscription accounts no longer start the official client. Order: a fresh UUID-matched `cachedUsageUtilization` snapshot (no request), then one read-only direct usage request with the live access token, inside the existing per-account reservation and `Retry-After`/429 backoff. subswap never refreshes the active credential.
- An expired active credential sends no request and reports `Claude credential expired; open Claude Code to renew`; Claude Code renews it on its next use.
- Account identity is checked before and after the request; a swap in between discards the reading.
- `native_usage.rs` and the "native empty reading" backoff branch were removed.

## Verification (2026-10-11)

- macOS ARM64: formatting, clippy `-D warnings`, full workspace tests, debug build and locked release build passed with no compiler or lint warnings. The new `cli_surface` test runs the default list with an expired active credential and a fake `claude` on `PATH`; it asserts the expired message, an untouched isolated keychain, and that the fake client never starts.
- v1.15.1 Release run passed CI on all three platforms, the Windows draft installer check, publication and the Homebrew formula update. Both Linux packages report a maximum of `GLIBC_2.28`.
- After the old 30-minute retry deadline passed, the installed 1.15.1 default list showed the active account's 5h and 7d readings from one direct usage response, with no `claude -p` process. The poll record stored the success with the three-minute floor. The re-login account still correctly showed `needs re-login`.

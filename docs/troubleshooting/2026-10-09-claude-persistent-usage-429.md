# Persistent Claude usage 429 with an expired active credential

## Scope and verified evidence

Investigation on 2026-10-09, initially using installed `subswap 1.14.6` and `Claude Code 2.1.295`, followed by a user-authorized implementation for subswap 1.15.0. Initial evidence and later runtime checks are distinguished below.

- The numbered default list showed two Claude subscription accounts: the inactive first account had a successful quota reading, while the active second account showed `quota 429 rate limited`.
- The active account's failure cache contained `consecutive = 39`, a usage `rate_limit_error`, and a last failure at `2026-10-09T01:57:00Z`. It had no successful quota entry. The counter is cached evidence, not a complete request history.
- Its access credential was nonempty and identical in the native credential file, the native macOS Keychain item, and subswap's credential store. Its declared expiry was `2026-10-08T10:40:15Z`, about 15 hours before inspection. The refresh credential was present and declared a future expiry; its actual validity was not tested. `user:profile` was present.
- Running `subswap --log debug` at approximately `02:00Z` reproduced the displayed failure, with unchanged failure time/count. For this account the CLI reused the existing failure during the 15-minute backoff; this run was **not** a new remote 429 measurement. The first account's quota refreshed successfully.
- Two native Claude processes were running. The user's statusline script only transformed stdin with `jq`; it made no HTTP requests. Session count alone does not establish request volume.
- The native configuration had no `cachedUsageUtilization` snapshot available to reuse. The initial investigation performed no logout, account swap, token refresh, configuration change, or direct usage probe.

No tokens, account identifiers, private addresses, or credential contents belong in this record.

## What explains the persistent display

The immediate cause of the unavailable reading is the last recorded usage-endpoint 429, retained during failure backoff, with no last-good quota to display. An independently verified obstacle is that all locally available copies of the active access credential were already expired.

The current query path accepts a nonempty access token without an expiry preflight. It only considers credential recovery after an authentication response, and it leaves active-account refresh to Claude Code. A 429 takes the transient-failure path instead, so it cannot establish credential validity or trigger recovery of an expired active login. Repeating the same stale credential after each cooldown can therefore leave the account unobservable. This explains a recovery gap; it does **not** prove that credential expiry caused the original remote 429.

Relevant verified source paths:

- `crates/providers/claude/src/lib.rs`: `ClaudeProvider::query_quota`, `read_active_credentials_if_matches`.
- `crates/providers/claude/src/oauth.rs`: `fetch_usage` sends `User-Agent: subswap/<version>` and preserves the HTTP status/body but drops response headers, including `Retry-After`.
- `crates/core/src/quota_cache.rs`: failures persist a timestamp/count/error; exponential backoff caps at the configured 15 minutes and does not retain a server deadline. Cache writes use a direct file write, not an account-scoped fetch reservation.
- `crates/cli/src/cmd/default.rs`: `fill_quotas_progressively` loads the shared cache before scheduling requests. CLI and daemon can share completed results, but sharing a cache alone does not prevent simultaneous requests or stale whole-file writes.

These were established properties of the initial implementation; the later implementation addresses them below. Their exact contribution to this account's first 429 remains unmeasured. No request-header experiment was run with the expired credential.

## Upstream findings and conflicting reports

1. **Usage throttling is distinct from exhausted model quota.** Reports [#30930](https://github.com/anthropics/claude-code/issues/30930) and [#31021](https://github.com/anthropics/claude-code/issues/31021) describe valid credentials and working model calls alongside persistent usage 429, sometimes with `Retry-After: 0`. The latter was closed as not planned; that is not evidence of a fix.
2. **Concurrent consumers can contend.** A reporter in [#77477](https://github.com/anthropics/claude-code/issues/77477) observed many CLI sessions keeping the usage endpoint throttled for hours. This supports reducing combined traffic; it does not prove that the two local sessions caused this incident. Proposed polling environment variables in that issue are requests, not documented settings to prescribe.
3. **There is no verified universal request allowance.** The historical subswap observation of roughly one request per minute is not an official service guarantee. [claude-swap's polling policy](https://github.com/realiti4/claude-swap/blob/3a4e5c14873eb5b32f182d55c68da98ac8c0db45/src/claude_swap/poll_policy.py) records hour-scale budgets, different account/token regimes, a three-minute normal floor, and slower recovery after throttling. Its own [issue #220](https://github.com/realiti4/claude-swap/issues/220) reports busy-account failures even after long inactivity, newly issued tokens, and different request identities. These are bounded field observations with conflicting scope, not a published Anthropic contract.
4. **Changing User-Agent is not a demonstrated cure.** A [usage-monitor report](https://github.com/Maciek-roboblog/Claude-Code-Usage-Monitor/issues/202) attributes persistent 429 to request identity. CodexBar's current source also uses a detected Claude version. However, #220 reports failure across both custom and native identities. Matching a header may matter in some deployments; it cannot be presented as this incident's established root cause or a guaranteed repair.
5. **The current public incident is a different surface.** At inspection, the [official status feed](https://status.claude.com/api/v2/incidents/unresolved.json) reported delayed Console/Admin API usage data; the [component feed](https://status.claude.com/api/v2/components.json) listed Claude API, claude.ai, and Claude Code as operational. This neither proves nor rules out an account-specific OAuth usage problem. Do not attribute the local 429 to the Console incident without matching evidence.

## Alternatives

| Approach | Benefit | Boundary and recommendation |
|---|---|---|
| Capture official statusline `rate_limits` | Server-reported 5h/7d readings already emitted during normal work; the collector adds no usage request | Preferred source for a running subscription session. Fields are optional, absent before the first response, and expire at their reset. Bind each reading to the session's actual account; never relabel it using a later global active account. It cannot refresh an idle account by itself. |
| Read Claude Code's own cached usage | No added network traffic, potentially richer windows | Opportunistic source only. Validate account UUID, timestamp, and reset boundaries. It is an internal format and was absent on this machine. |
| Ask the official CLI for structured usage | Native client owns credential recovery and response interpretation | Promising on-demand source. The SDK method is explicitly experimental; `get_usage` still reaches the same usage service when it cannot use cached data. It is not a 429 bypass and must share the collector's cooldown. Runtime recovery of this account was not tested. |
| Improve direct OAuth polling | Can observe parked accounts without making inference requests | Retain as a measured fallback: check expiry first, serialize per account, atomically persist results/deadlines, honor server `Retry-After`, use conservative cadence and jitter, and preserve honest unknown/stale states. Do not assume a 90-second interval or 15-minute cap fits every account. |
| Web usage with an existing browser session | Separate browser authentication and the same account-wide usage view | Optional recovery/display source, not a universal automatic fallback. Requires matching browser identity, protected cookie storage, and handling expiry/Cloudflare challenges. Avoid adding browser-cookie access merely to work around a 429. |
| Local transcript analysis such as ccusage | Useful historical token/cost accounting without quota polling | Cannot establish remaining subscription quota across web, desktop, and other machines. It must not drive automatic switching as if it were the server's quota. |
| Artificial one-token model probes | Some reports derive 5h/7d readings from response headers while usage is throttled | Consumes quota and may omit other windows; direct request compatibility is fragile. Not recommended for routine background collection. Reuse naturally occurring official-client data instead. |
| Public Usage and Cost API | Supported organization API usage/cost reporting | Wrong product boundary for individual Pro/Max remaining 5h/7d allowance. Do not substitute Console costs for subscription quota. |

The [official statusline contract](https://code.claude.com/docs/en/statusline#available-data) documents the 5h/7d fields and when they may be absent. The [official SDK changelog](https://github.com/anthropics/claude-agent-sdk-typescript/blob/main/CHANGELOG.md#03169) introduced `usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET()` in 0.3.169. Installed Claude Code 2.1.295 contains that control path; symbol presence is compatibility evidence, not a successful live quota measurement.

[ccusage](https://ccusage.com/guide/) reads local history. [Official usage-limit guidance](https://support.claude.com/en/articles/11647753-how-do-usage-and-length-limits-work) explains that Claude product surfaces share a subscription allowance. The [Usage and Cost API documentation](https://platform.claude.com/docs/en/manage-claude/usage-cost-api) covers organization API reporting and separate Enterprise analytics. These sources distinguish historical usage/cost from remaining individual-plan capacity.

## Source inspections and reuse boundaries

Four repositories were shallow-cloned outside the workspace and their relevant implementations inspected; none was executed against local credentials or copied into the product.

| Reference and revision | Inspected behavior | Applicable / not copied |
|---|---|---|
| [CodexBar](https://github.com/steipete/CodexBar), `61fb1a8ad33e6af5ebdc973ed55f90baff1ea415` | `ClaudeOAuthUsageFetcher`, `ClaudeOAuthUsageRateLimitGate`, `ClaudeUsageFetcher`, provider documentation | Parses seconds/date `Retry-After`, persists token-fingerprint cooldown, and keeps 429 terminal. Reuse the deadline/error separation; do not copy manual cooldown bypasses, credential-owner policy, or fallback chains blindly. |
| [claude-swap](https://github.com/realiti4/claude-swap), `3a4e5c14873eb5b32f182d55c68da98ac8c0db45` | `poll_policy.py`, `usage_store.py`, `oauth.py` | Shared adaptive cadence and fenced fetch leases are useful. Its [claim-lifetime bug](https://github.com/realiti4/claude-swap/issues/157) shows a lease must cover queueing, refresh, and request duration. Do not copy unverified limiter constants or treating old below-threshold quota as proof of a usable target. |
| [Claude Codex Usage Monitor](https://github.com/Sundogs-s/Claude-Codex-Usage-Monitor), `6b855bcfeaa91b69b77fbe20542c84cf46a45ae3` | `src/claude.rs`, `src/monitor.rs` | Delegates expired credentials to native `get_usage` without owning refresh rotation. Its direct polling path discards `Retry-After`, so it is a protocol example, not the preferred complete collector. Do not copy its model-probe fallback. |
| [TokenMonitor](https://github.com/Michael-OvO/TokenMonitor), `8f03dbe67861610ed14bc19dc89c96910cb3ca0c` | `rate_limits/claude.rs`, `statusline/source.rs`, README | Prefers emitted official readings and validates the native cache's account UUID. Reuse the source-priority/identity concepts; do not copy its statusline installation, raw payload retention, or UI. |

## Proposed direction and verification boundary

### Authorized implementation contract

The user requested implementation after reviewing the investigation on 2026-10-09. Active-account queries must prefer account-validated official cached readings and then delegate structured usage and expired-credential recovery to Claude Code. They must never refresh the active credential independently or fall back to a second HTTP request after a native usage failure. Parked accounts retain direct OAuth access with expiry preflight and guarded refresh. All network collection paths must share a durable per-account reservation, recent result, and retry deadline across CLI and daemon. Server retry deadlines take precedence over the generic failure cap. Native protocol incompatibility, absent data, expired credentials, and rate limiting remain distinguishable failures; unknown quota must not become a usable auto-swap candidate. No synthetic model prompts or browser-cookie access are introduced. Passive statusline capture is deferred until session-to-account attribution can be established without modifying an existing user script or trusting the later global active account.

### Implementation and initial runtime result

- `native_usage.rs` performs a version-gated initialized control session and sends only `get_usage` with `skip_behaviors`; it disables hooks, MCP startup and session persistence. No model message is sent. Before/after account identity is checked, and native credential changes are captured back to the store.
- `cached_native_usage` accepts only a fresh matching email/account UUID snapshot and excludes reset or missing/invalid utilization windows. Statusline scripts and browser cookies remain untouched.
- `usage_poll.rs` stores independent per-account reservations, results and deadlines with file locking and atomic replacement. A reservation is persisted before network activity, so cancellation and process exit retain the query floor. The default floor is three minutes; 429 or a native empty reading has a 30-minute conservative minimum. A later `Retry-After` deadline wins. Parked quota refresh and daemon keepalive share a separate credential-rotation lock and reread credentials inside it. Manual activation remains independent of quota collection.
- An initial native control probe at `2026-10-09T02:12:26Z` returned `rate_limits_available: true` and `rate_limits: null`, with zero model cost/API duration. Native file and Keychain expiry remained unchanged afterwards. This probe did not establish a new 429 response or successful credential recovery; it motivated keeping null results distinct from throttling and performing SDK initialization in the implemented path.
- Local unit/integration checks cover initialized control messages without inference/history scanning, account/cache expiry validation, seconds/date/overflow `Retry-After`, durable cancellation reservation, cross-handle exclusivity, and deadlines exceeding the generic cap. Final installed-account acceptance is recorded separately when completed.

The implemented source order prioritizes an account-validated native snapshot and a version-gated native query when needed. Passive statusline collection remains a future improvement with the identity boundary above. `Retry-After: 0` or a missing header uses a conservative fallback. [RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#name-retry-after) defines both delta-seconds and HTTP-date forms. Server deadlines are not truncated by the generic backoff cap.

For this incident, the next discriminating check is one native-client usage query after cooldown, inspecting whether the official client renews the credential and whether the returned quota belongs to the intended account. A fresh credential followed by the same 429 would separate credential recovery from upstream usage throttling. Do not refresh active credentials independently, remove working login state, or run alternating header/channel probes.

Verified initial evidence: installed default-list behavior, cache/backoff reuse, local credential equality/declared expiry/scope, statusline's lack of network activity, four reference implementations, current official documentation/status reports, and a native empty control response. Not established by that evidence: the original trigger, real remaining quota, remote refresh-credential validity, effects of other machines, or successful native recovery. Product code was subsequently changed as authorized; local account selection, statusline configuration and browser sessions were not changed.

# Pi extension design

## Adopted product requirement (2026-11-07)

Subswap must provide a Pi extension that uses Subswap's existing provider/account inventory for same-provider account failover. Two Codex accounts and two Volcengine Coding Plan keys are the immediate examples, not separate one-off integrations. The extension replaces the separate `pi-multi-account` failover owner after the new path passes acceptance; users should not need a second account database or another multi-account plugin.

The following boundaries are adopted with that requirement:

- Subswap remains the credential and account-metadata owner. Pi must not persist duplicate OAuth refresh tokens or API keys, and provider-specific token refresh remains inside Subswap's existing coordination rules.
- Failover stays within one Subswap provider and preserves the requested model, thinking level, conversation, and original task. It reacts to real inference responses; it does not probe upstream services or manufacture quota errors.
- Pi request-scoped failover is separate from daemon/background AutoSwap. It may use manually selectable API-key accounts in the same pool when their real request fails, including Volcengine keys marked `manual_only` for background AutoSwap. This does not relax `manual_only` in the Rust AutoSwap policy.
- Existing model choices and the configured default remain usable during migration. The old failover owner is removed only after ordinary inference and an isolated forced-failover path pass on the new extension.
- Credentials, credential blobs, authorization headers, and secret subprocess output must never enter logs, notifications, session entries, tests, or Git.
- **Owner correction (2026-11-07):** the extension must be completely transparent to the user. Do not inject prompts or system context, register chat commands, emit failover notifications, or print plugin errors/status in Pi. Failover happens only inside the native request stream. Unrecovered failures keep Pi's native behavior without adding extension-authored output.

## Implementation direction

The extension will reuse Pi's native provider transports and Subswap's structured read/write interfaces where possible. Account-specific Pi providers are only projections of Subswap accounts, not new credential owners. A small extension-owned session state tracks accounts already tried during one failure cascade to prevent A→B→A loops.

References inspected before implementation:

- [`pi-multiprovider`](https://github.com/monotykamary/pi-multiprovider), revision `19c30e8abfafb19c24dd4dbfd19bf2aacc22f433`, especially `src/lift.ts` and `tests/lift.spec.ts`. Reuse its native-stream wrapping, per-attempt response capture, delayed `start` event, no-replay-after-output rule, and single retry owner. Do not copy its independent credential store, account scheduler, UI, or virtual-provider layer.
- Installed `pi-multi-account` 1.28.1 ([package documentation](https://pi.dev/packages/pi-multi-account)) and the existing local `subswap-codex` provider. Reuse native-provider composition and request-time access-token resolution. Do not copy the independent account registry, long-lived probe scheduler, duplicate credential storage, model-switch continuation, or monkey patches.

The CLI credential projection is an intentional local IPC boundary: it returns only the selected account's current access token or API key to the calling Pi process, never refresh tokens. It is not a general status/log output and must be documented and tested as secret material.

## Acceptance contract

1. Static checks and automated tests cover account discovery, secret redaction, same-pool candidate selection, loop prevention, quota-error matching, and preservation of unrelated errors.
2. A normal installed Pi invocation lists and uses the projected accounts without an explicit extension path.
3. An isolated Pi configuration injects a quota refusal for the first account and verifies that the original prompt continues on a sibling account with the same model and thinking level.
4. Real inference succeeds through the installed extension for both the Codex and Volcengine pool shapes, without printing credentials. (Status: both real pools were quota-exhausted at acceptance, so this item remains pending quota reset; the isolated mock failover and the real upstream quota-error surfacing below are the current evidence, not a live-success claim.)
5. Only after those checks does machine configuration remove `pi-multi-account` and retire duplicate provider/credential projections. Post-install commands and a fresh user-visible Pi surface verify the final loaded state.

## Implementation status (2026-11-07)

Delivered in `integrations/pi/`:

- `subswap-codex` reuses Pi's native Codex provider catalog and stream (8 models on Pi 1.5.0; the count follows Pi releases); `subswap-volcengine` registers the official Coding Plan OpenAI-compatible endpoint (`.../api/coding/v3`, distinct from the native Claude path `.../api/coding`) with `kimi-k3.1-pro`.
- One native stream wraps each pool. It buffers `start`, tries accounts in active / most-recently-healthy order, rotates only before visible output on a confirmed account failure, and preserves model, thinking level, transcript, headers, and user task. Pi provider retries are forced to `0` for a pooled attempt.
- `manual_only` API-key accounts participate in request-scoped failover; Rust daemon AutoSwap policy is unchanged.
- No commands, prompt/system injections, notifications, widgets, or extension-authored error output. A later unrecoverable failure is surfaced through Pi's normal error path.
- On `session_start`, a model that still names a removed alias (`coding-plan`, `coding-plan-2`, `codex-1`, `codex-2`) is silently mapped to the same model in the corresponding pool when present. This is a one-time migration, not a failover decision. The automatic switch sets `PI_MODEL_MEMORY_SUPPRESS_UNTIL` so model-memory does not persist it as a user default; lookup or switch failures keep the prior selection without any extension error output.
- `subswap auth <provider>/<id>` returns exactly one JSON object: `kind=credential` with an access token and optional expiry, or `kind=environment` with the Volcengine env projection. Ordinary mode is read-only during projection: it never returns a refresh token, never switches active, never queries quota, and performs no active/quota/refresh/registry/cache/audit writes. Documented exceptions are fresh-home directory creation (`AppPaths::resolve`) and a one-time legacy-layout migration (lazy keyring backfill, best-effort legacy OpenCode API-key migration); neither is triggered by the projection itself on a settled home. CLI logs go to stderr so stdout stays one JSON object. Near-expiry renewal uses `subswap auth <provider>/<id> --renew`, which only renews that one account (parked via existing direct OAuth with dead-token guards; active Codex only via official app-server coordination with identity preflight, refusing conservatively when coordination is unavailable) and still never touches quota, the default entry, AutoSwap, registry active, or subswapd. Unsupported providers fail explicitly.
- The Pi client caches an API key for a short TTL and a Codex access token until shortly before expiry; a failed attempt invalidates that account's entry before a later retry. Renewal calls are deduplicated per account while in flight and throttled to one attempt per account per 90 seconds; unreadable command output surfaces as a generic error without secret content. Accounts added or removed while Pi is running require `/reload` or a new session; a removed account fails its next setup gracefully through the per-account cooldown path.

Machine configuration after acceptance:

- `~/.pi/agent/extensions/subswap-pi` links to this checkout.
- The migration set the pool default to `subswap-volcengine/kimi-k3.1-pro`. The live default is user-owned: `/model` plus model-memory may persist a later choice, so do not treat the migrated default as a fixed current fact. Both pools stay enabled and scoped: `subswap-codex/gpt-6.1-sol`, `subswap-volcengine/kimi-k3.1-pro`, and `deepseek/*`.
- `pi-multi-account`, the `subswap-codex` alias extension, the `codex-1` / `codex-2` discovery markers, the static `coding-plan` / `coding-plan-2` providers and keys, and the stale `~/.pi/agent/provider-failover*` runtime state were removed (verified absent; a fresh `pi --list-models` does not recreate them). Subswap is the sole owner of the Codex and Volcengine credentials. Already-running Pi processes keep their loaded code and still need `/reload` or a restart; on-disk removal alone does not migrate them.

Verification:

- `cargo test --workspace` passed, including 8 `auth_projection` integration tests (Codex access-only output, Volcengine environment output, `--renew` freshness window, parked/active renewal boundaries), 29 shared-engine tests, and 49 Codex provider tests; strict `cargo clippy --workspace --all-targets -- -D warnings` passed.
- `integrations/pi`: strict `tsc --noEmit` passed; 28 node tests passed, covering same-pool rotation, no replay after output, loop prevention, manual-only keys, abort handling, conservative error classification, caching/invalidation, per-account renewal dedup and throttle, unreadable-output redaction, legacy alias migration with model-memory suppression, and secret-free errors.
- A fresh installed `pi --list-models` listed `subswap-codex` and `subswap-volcengine` without an explicit extension path. At acceptance, a default `pi -p` request selected `subswap-volcengine/kimi-k3.1-pro` and surfaced the real upstream 5-hour quota 429 rather than an unknown-provider or plugin error. (The live default is user-owned and may since differ; this records the acceptance-time behavior.)
- Full-chain failover acceptance used a temporary Subswap home with the active Volcengine key replaced by an invalid key, a local OpenAI-compatible mock, and the real `subswap` binary. The mock observed exactly `invalid` then `valid`; Pi exited `0` with `MOCK_OK` and no stderr.

Residual limit at acceptance: both real account groups were genuinely exhausted (Volcengine 5-hour window 100% on both keys; Codex 7-day window 100% on both accounts), so successful live inference through each real pool is still pending quota reset. The real requests reached the upstream provider and returned its quota error; the isolated failover proof establishes the rotation path. Cursor, OpenCode, Claude OAuth, Kimi, and Command Code are not exposed by this version; they require separate adapters.

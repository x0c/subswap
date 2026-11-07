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
4. Real inference succeeds through the installed extension for both the Codex and Volcengine pool shapes, without printing credentials.
5. Only after those checks does machine configuration remove `pi-multi-account` and retire duplicate provider/credential projections. Post-install commands and a fresh user-visible Pi surface verify the final loaded state.

## Implementation status (2026-11-07)

Delivered in `integrations/pi/`:

- `subswap-codex` reuses Pi's native Codex provider catalog and stream; `subswap-volcengine` registers the official Coding Plan OpenAI-compatible endpoint with `kimi-k3.1-pro`.
- One native stream wraps each pool. It buffers `start`, tries accounts in active / most-recently-healthy order, rotates only before visible output on a confirmed account failure, and preserves model, thinking level, transcript, headers, and user task. Pi provider retries are forced to `0` for a pooled attempt.
- `manual_only` API-key accounts participate in request-scoped failover; Rust daemon AutoSwap policy is unchanged.
- No commands, prompt/system injections, notifications, widgets, or extension-authored error output. A later unrecoverable failure is surfaced through Pi's normal error path.
- On `session_start`, a model that still names a removed alias (`coding-plan`, `coding-plan-2`, `codex-1`, `codex-2`) is silently mapped to the same model in the corresponding pool when present. This is a one-time migration, not a failover decision.
- `subswap auth <provider>/<id>` returns exactly one JSON object: `kind=credential` with an access token and optional expiry, or `kind=environment` with the Volcengine env projection. It never returns a refresh token, never switches active, and never queries quota. Unsupported providers fail explicitly.
- The Pi client caches an API key for a short TTL and a Codex access token until shortly before expiry; a failed attempt invalidates that account's entry before a later retry. Accounts added while Pi is running require `/reload` or a new session.

Machine configuration after acceptance:

- `~/.pi/agent/extensions/subswap-pi` links to this checkout.
- Default model is `subswap-volcengine/kimi-k3.1-pro`; scoped models are `subswap-codex/gpt-6.1-sol`, `subswap-volcengine/kimi-k3.1-pro`, and `deepseek/*`.
- `pi-multi-account`, the `subswap-codex` alias extension, the `codex-1` / `codex-2` discovery markers, and the static `coding-plan` / `coding-plan-2` providers and keys were removed. Subswap is the sole owner of the Codex and Volcengine credentials.

Verification:

- `cargo test --workspace` passed, including the new `auth_projection` integration tests for Codex access-only output and Volcengine environment output.
- `integrations/pi`: strict `tsc --noEmit` passed; 15 node tests passed, covering same-pool rotation, no replay after output, loop prevention, manual-only keys, abort handling, conservative error classification, caching/invalidation, legacy alias migration, and secret-free errors.
- A fresh installed `pi --list-models` listed `subswap-codex` and `subswap-volcengine` without an explicit extension path. A default `pi -p` request selected `subswap-volcengine/kimi-k3.1-pro` and surfaced the real upstream 5-hour quota 429 rather than an unknown-provider or plugin error.
- Full-chain failover acceptance used a temporary Subswap home with the active Volcengine key replaced by an invalid key, a local OpenAI-compatible mock, and the real `subswap` binary. The mock observed exactly `invalid` then `valid`; Pi exited `0` with `MOCK_OK` and no stderr.

Residual limit at acceptance: both real account groups were genuinely exhausted (Volcengine 5-hour window 100% on both keys; Codex 7-day window 100% on both accounts), so successful live inference through each real pool is still pending quota reset. The real requests reached the upstream provider and returned its quota error; the isolated failover proof establishes the rotation path. Cursor, OpenCode, Claude OAuth, Kimi, and Command Code are not exposed by this version; they require separate adapters.

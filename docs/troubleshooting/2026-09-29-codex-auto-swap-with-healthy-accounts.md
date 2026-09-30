# Healthy-account automatic swaps across providers (2026-09-29)

## Symptom and scope

The default `subswap` entry prints `auto: swapped` even though the account active before the command and the selected account both finish with usable 5-hour and 7-day quota. Repeated invocations can select alternating accounts. This is a decision-order defect in the default entry, separate from a successful swap that an already-running Codex session does not observe; see [the running-session record](2026-09-11-codex-swap-requires-restart.md).

## Verified reproduction

On macOS with the installed v1.9.2 binary, no `config.toml` override, and the compiled 99% **used** threshold, one default-entry run began with account A active. The local quota cache and audit log recorded this order (UTC):

| Event | Time | Final quota in that response |
|---|---|---|
| Inactive account B quota became ready | 05:59:25.883 | 5h 56% left; 7d 21% left |
| `auto_swap` to B succeeded | 05:59:25.886 | A was still loading |
| Formerly active account A quota became ready | 05:59:28.441 | 5h 96% left; 7d 22% left |

The command displayed B as active and the restart notice. Neither final quota met the 5-hour threshold, and neither 7-day window was exhausted. The audit also shows earlier alternating swaps, but their exact quota completion order was not retained; this reproduction establishes the cause for this one run, not every earlier swap.

A post-install v1.9.3 default-entry check reproduced the same order: B quota ready at 06:13:00.745Z (5h 11% left), successful swap at 06:13:00.749Z, then A quota ready at 06:13:02.186Z (5h 80% left). v1.9.3 changes documentation and the version only; this behavior remains unfixed.

## Cause

`crates/cli/src/cmd/default.rs::fill_quotas_progressively` queries accounts concurrently and calls `try_auto_swap_ready_provider` after **each** result. `crates/core/src/auto_policy.rs::decide` may select a known-available candidate while the current account's quota remains `Loading` after its settle grace. The activation happens before the current account's own result arrives. Once B is active and known available, the later healthy result for A does not restore A. Thus an account response race can bypass the intended quota threshold without any exhausted account.

## Diagnostic and repair boundary

- Treat `auto: swapped` as evidence that the global credential changed, not as evidence that the previous account was depleted. Compare the per-account cache `cached_at` values with the `auto_swap` audit event before attributing the trigger. Do not use rapid usage requests to reproduce it.
- The present macOS default also leaves `subswapd` stopped unless `SUBSWAP_AUTO_DAEMON=1` is set. That explains why a swap may appear only when the user runs `subswap`; it does not explain **why** a healthy account was selected. See [CLI behavior](../CLI.md) and [auto-swap design](../design/AUTO_SWAP_DESIGN.md).
- **Corrected in v1.11.1 (2026-09-30):** the shared policy preserves `Loading` active accounts indefinitely; `Failed`/`Stale` active results degrade without changing credentials. Automatic activation requires a confirmed trigger and a completed usable candidate result. Failed/unknown-target escape and depleted-target earliest-reset fallbacks have been removed. Cursor unknown pools no longer count as exhausted. Manual swap remains independent of quota queries.

## OpenCode reproduction and verification

Before the correction, an installed v1.11.0 default-entry run selected the other OpenCode Console account although both accounts retained 98–100% of their hourly quota and usable weekly/monthly quota. This reproduced the same visible healthy-account swap already observed for Codex. The shared policy's candidate-first `Loading` branch explains the behavior; the exact request timestamps were not retained for this OpenCode run.

Regression coverage includes a controlled candidate-first OpenCode default-entry race, waiting for a usable target without a failed-account intermediate jump, and a provider-wide matrix for loading, timeout/429/401, stale active/target quotas, unknown status, and zero limits. Existing confirmed-exhaustion, Cursor parallel-pool, manual-only, and manual-hold cases remain required. Local validation passed 305 tests plus workspace check/build and the locked release build. After committing the correction, both installed binaries matched their release SHA-256 and `subswap --version` reported 1.11.1. The daemon was restarted from the installed binary, and two consecutive real default-entry invocations retained the same active accounts without `auto: swapped`. The first OpenCode check also reported an existing manual hold; the controlled candidate-first test independently verifies the corrected policy without that hold. No real account was deliberately depleted or hammered to manufacture a rate limit.

<!-- 该文档整理/压缩于 2026-09-29 -->

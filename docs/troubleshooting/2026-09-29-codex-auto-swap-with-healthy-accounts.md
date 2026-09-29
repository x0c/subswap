# Codex auto swap while both accounts have quota (2026-09-29)

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

## Cause

`crates/cli/src/cmd/default.rs::fill_quotas_progressively` queries accounts concurrently and calls `try_auto_swap_ready_provider` after **each** result. `crates/core/src/auto_policy.rs::decide` may select a known-available candidate while the current account's quota remains `Loading` after its settle grace. The activation happens before the current account's own result arrives. Once B is active and known available, the later healthy result for A does not restore A. Thus an account response race can bypass the intended quota threshold without any exhausted account.

## Diagnostic and repair boundary

- Treat `auto: swapped` as evidence that the global credential changed, not as evidence that the previous account was depleted. Compare the per-account cache `cached_at` values with the `auto_swap` audit event before attributing the trigger. Do not use rapid usage requests to reproduce it.
- The present macOS default also leaves `subswapd` stopped unless `SUBSWAP_AUTO_DAEMON=1` is set. That explains why a swap may appear only when the user runs `subswap`; it does not explain **why** a healthy account was selected. See [CLI behavior](../CLI.md) and [auto-swap design](../design/AUTO_SWAP_DESIGN.md).
- **Unfixed:** the implementation still treats an active `Loading` state as a reason to leave it. A future correction needs to preserve prompt escape from confirmed exhaustion while ensuring a healthy active account is not replaced merely because another query returned first. Verify the default entry and daemon paths separately. The exact waiting/failure policy remains a design decision.

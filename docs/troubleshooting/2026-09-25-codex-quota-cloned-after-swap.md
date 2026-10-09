# 2026-09-25 — Codex accounts briefly show identical quotas after a swap

## Symptom and live evidence

Two distinct Codex accounts briefly displayed the same 5-hour and 7-day remaining percentages and reset times. A later `subswap` run showed the active account at 42% weekly remaining and the parked account at 0%, resetting on September 26 at 16:23 China time. Their stored Codex account IDs and token fingerprints were distinct. The later result rules out a permanent duplicate registry entry; the earlier response itself was not retained, so its exact upstream source cannot be reconstructed.

## Confirmed defect and likely path

`fetch_codex_quota` preferred the persistent Codex app-server control socket for the active account. `rate_limits_to_usage` previously discarded the response's `accountId`, and `fetch_codex_quota` assigned the current registry account ID to every returned window. A Codex process started before a swap can retain the old login while the live `auth.json` already names the new account. In that state, the old account's quota can be cached and displayed under the new account. This path explains the transient matching rows; the historical response was not captured, so that attribution remains an inference.

## Fix and verification

Require the app-server response `accountId` to match the selected account's `chatgpt_account_id`. Reject missing or mismatched IDs and use the compatible read-only query directly. Since 1.14.6 the control socket uses WebSocket and preflights optional `workspaceRouting.chatgptAccountId`, rejecting known stale daemons before querying quota. Never cache an unverified result under the selected account. Tests cover matching, different, and missing IDs, with and without preflight identity. The official [app-server response schema](https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/schema/json/v2/GetAccountRateLimitsResponse.json) defines the optional `accountId` field.

The separate issue of an already running Codex CLI continuing to use the previous account is covered by [the restart investigation](2026-09-11-codex-swap-requires-restart.md). Quota display correctness does not change the login held in an existing process.

<!-- 该文档整理/压缩于 2026-09-29 -->

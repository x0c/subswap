# `subswap rm` refused active custom API / manual-only account

## Symptoms

- `subswap rm <N>` exits non-zero with
  `cannot remove active manual-only account claude/...; swap away first`
- User cannot delete the only / current Claude custom API profile without a second account to swap to.

## Cause

Early custom-API support blocked removing the active `manual_only` account so settings would not stay pointed at a deleted API key. `disconnect_official` for API accounts returned `AlreadyGone` without restoring `settings.json` or clearing `.subswap-api.json`.

## Remedy

`rm` never refuses for active / `manual_only`. Removing the active Claude API account exits API mode first (restore managed env, delete the API sentinel), then clears registry and store. Official client disconnect failures only warn; local removal still proceeds (a still-signed-in client may re-import on the next default entry).

## Verification

- Isolated CLI test: add DeepSeek API → swap to it → `rm` succeeds; settings restore prior env; `.subswap-api.json` gone; registry no longer lists the API id.
- Live: with an active custom API as `*`, `subswap rm <N>` prints `removed ...` and the status table without asking to swap away.

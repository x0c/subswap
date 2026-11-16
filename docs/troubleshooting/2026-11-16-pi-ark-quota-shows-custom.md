# Pi drawer Ark accounts show `custom` with no quota windows (2026-11-16)

## Symptoms

1. The default entry prints the Ark (火山方舟 / Volcengine Coding Plan) accounts under `pi` with the bare
   `custom` tag instead of `5h` / `7d` / `mo` windows, while the `opencode-go` keys in the same drawer still show windows.
2. No error text accompanies the row: `subswap status --json` reports the account with an empty `quotas` array and
   `fetch_state: stale`, and `--refresh` does not change it.
3. Pointing `SUBSWAP_VOLCENGINE_OPENAPI_BASE` at a dead endpoint changes nothing, which proves no request is attempted.

## Root Cause

1. **Missing Pi→Volcengine quota wiring in the installed build.** The Pi migration moved Ark accounts out of the
   top-level `volcengine` provider, and that migration release dropped the quota path entirely: `PiProvider::query_quota`
   only handled the OpenCode channels and returned `Ok(Vec::new())` for everything else. The collector therefore records
   `state=success` with `quotas: []` — a silent empty result, not a failure. In this state a stale build is doubly
   confusing: the daemon (same stale binary) writes the empty success into `quota_cache.json`, so a freshly built CLI
   still renders `custom` until the 90s cache floor expires and it queries again itself.
2. **Independent second cause: no quota credentials.** Per-account `access_key_id` / `secret_access_key` are stored
   separately from the inference key (`/pi:<id>:access_key_id`, `/pi:<id>:secret_access_key` in `credentials.json`), and
   the provider deliberately returns an empty result when either half is missing — it never invents numbers. An earlier
   migration deleted those fields outright; `backfill_volcengine_quota_credentials` now recovers them from
   `volcengine/<id>` residue when it still exists.

Diagnosing which of the two applies, without touching the network: check the per-account collector record
`~/Library/Caches/dev.subswap.subswap/quota_collection/pi-<hash>.json`. `state=success` + `quotas: []` means the provider
short-circuited (channel mismatch or missing AK/SK); a real transport/API problem lands as `state=failure` with an
`error` string instead.

## Remedy

1. Move the V4 signing plus `GetCodingPlanUsage` / `GetAFPUsage` calls into `crates/providers/common/src/volcengine_quota.rs`
   (shared with the read-only top-level `volcengine` provider, parameterized by the caller's provider id).
2. Dispatch `PiProvider::query_quota` by `account.extra.channel`: the standard Volcengine channel
   (`auth::volcengine_channel()`) queries with that account's AK/SK, `opencode-go*` keeps using `common::opencode_go_usage`,
   and every metered channel stays empty.
3. Keep the quota pair across the legacy migration and backfill accounts an earlier migration stripped.
4. Attach or replace the pair with `subswap login volcengine -- --ak-id <AK> --ak-secret <SK>` (existing slot account) or
   `subswap add-api pi --preset volcengine --ak-id <AK> --ak-secret <SK>` (new key).

## Verification

- `cargo test --workspace` on the merged tree, plus `cargo build --locked --release -p subswap-cli -p subswap-daemon`.
- Installed both binaries, restarted `subswap __daemon`, then cold-cleared the two Ark collector records and ran the
  installed `subswap`: both Ark rows return `5h` / `7d` / `mo` windows from live `GetCodingPlanUsage` responses, and the
  values survive a daemon refresh cycle instead of flipping back to `custom`.
- `subswap status --refresh --json` shows the same windows with `fetch_state: ready`.

# Antigravity missing Gemini weekly (`7d`) quota or showing unlabeled `--` windows (2026-11-14)

## Symptoms

1. Running `subswap` prints unlabeled `5h [100% left ...]` / `7d [100% left ...]` next to a cryptic `-- [73% left ...]`, making it impossible to tell which pool (`Gemini` vs `Claude/GPT`) is depleted.
2. After separating pools onto sub-rows, `Claude/GPT` shows both `5h` and `7d`, while `Gemini` shows only `5h` and completely omits `7d`.
3. Untouched `100% left` buckets report `reset in 5h` / `reset in 7d` on every refresh instead of `reset --`.

## Root Causes

1. **`User-Agent` version gate on `retrieveUserQuotaSummary`**: Google's `v1internal:retrieveUserQuotaSummary` endpoint filters returned quota groups by the client version in `User-Agent`. Sending legacy strings such as `Antigravity/1.18.3` or `antigravity/hub/1.0.0` causes both `daily-cloudcode-pa.googleapis.com` and `cloudcode-pa.googleapis.com` to **omit the `Gemini Models` (and `Gemini 4 Argon`) group completely** and return only `"Claude and GPT models"` (`3p-5h`, `3p-weekly`). That forced fallback to `v1internal:fetchAvailableModels`, which previously lacked weekly quota parsing and mapped model buckets to `QuotaWindow::Custom` (`--`). Upstream reference: [pingdotgg/t3code#28090](https://github.com/pingdotgg/t3code/pull/28090).
2. **Invalid proto3 `Client-Metadata` enum**: `agy`'s embedded protobuf descriptor defines `ClientMetadata.Platform` as `PLATFORM_UNSPECIFIED`, `DARWIN_AMD64`, `DARWIN_ARM64`, `LINUX_AMD64`, `LINUX_ARM64`, `WINDOWS_AMD64` — `"MACOS"` is not a valid enum name.
3. **Synthetic rolling `resetTime` on untouched summary buckets**: For untouched buckets (`remainingFraction == 1.0` with empty `description`), `retrieveUserQuotaSummary` synthesizes `resetTime = now + window` on every call rather than omitting `resetTime`.
4. **Unlabeled multi-pool duplicate windows**: Unlike single-pool providers, one Antigravity account simultaneously owns independent `Gemini` (plus optional `Gemini 4 Argon` / `Carbon`) and `Claude/GPT` pools, each with its own `FiveHour` and `SevenDay` windows. Rendering flat `5h`/`7d` tokens or exporting duplicate `"five_hour"` JSON keys obscures pool identity and collides in client `QuotaSnapshot` IDs.

## Remedy

1. Send `User-Agent: antigravity/hub/2.24.0 (aidev_client; os_type=<platform>; arch=<arch>; cl=963137146)` and `Client-Metadata` with `"platform":"PLATFORM_UNSPECIFIED"` in `crates/providers/antigravity/src/quota.rs`.
2. Order official endpoints by `auth_method` (`"business"` → `prod` then `daily`; default `"consumer"` → `daily` then `prod`), send `{"project": <id>}` only when an explicit project ID exists (`{}` otherwise), and stop on the first host returning both `Gemini` and `Claude/GPT` groups.
3. Preserve `gemini-argon-*` / `gemini-carbon-*` buckets as separate pools instead of merging them into default `Gemini`; clear synthetic `now + window` `reset_at` timestamps on untouched `100%` buckets; parse `weeklyQuotaInfo` alongside `quotaInfo` in `fetchAvailableModels` fallback without ever emitting `QuotaWindow::Custom`.
4. Prefix `Quota.note` with canonical pool group names (`Quota::pool_group()`), render multi-pool accounts on aligned per-pool sub-rows in `crates/cli/src/render.rs`, and disambiguate duplicate `status --json` windows as `<pool>_<window>` (`gemini_5h`, `gemini_7d`, `claude_5h`, `claude_7d`) in `crates/cli/src/cmd/status.rs`.

## Verification

- Unit tests in `crates/providers/antigravity/src/quota.rs`, `crates/cli/src/render.rs`, and `crates/cli/src/cmd/status.rs`.
- Live installed `subswap --log debug` after the 90s `quota_cache.json` floor expired confirmed a single round-trip to `https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary` returning all four windows (`Gemini 5h` + `7d` and `Claude/GPT 5h` + `7d`) with zero fallback requests.

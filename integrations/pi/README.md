# Subswap Pi integration

Pi providers backed by Subswap's account pools. Each supported Subswap provider (currently `codex` and `volcengine`) becomes one Pi provider, so all accounts in that group share the same models and fail over inside a single request.

Supported in this version:

| Subswap provider | Pi provider | Models |
|---|---|---|
| `codex` | `subswap-codex` | Pi's native Codex catalog |
| `volcengine` | `subswap-volcengine` | `kimi-k3.1-pro` |

## Behavior

- The active or most recently healthy account is tried first.
- A real quota, rate-limit, or account-auth failure before any text, thinking, or tool-call output rotates to the next account in the same group and retries the same request.
- Model, thinking level, original conversation, headers, and user task are preserved. Pi's provider-level retries stay at `0`; this extension is the only retry owner for a pooled request.
- After output becomes visible, the error is surfaced unchanged. The extension never replays an already-visible turn.
- The extension has no chat commands, prompts, notifications, widgets, or status rows. Unrecovered errors keep Pi's native presentation.
- On session start, a model still pointing at a removed alias (`coding-plan`, `coding-plan-2`, `codex-1`, `codex-2`) is silently mapped to the same model in the corresponding pool. If that model is unavailable, the selection is left untouched. The automatic switch sets `PI_MODEL_MEMORY_SUPPRESS_UNTIL` so it is not persisted as a user default; a failed lookup keeps the prior selection without any extension error output.
- Repeated attempts are bounded to once per account per request. Failed accounts cool down using `Retry-After` when present, otherwise a conservative default.
- Manually selectable Subswap API-key accounts, including `manual_only` accounts such as Volcengine Coding Plan keys, participate in request-scoped failover. This does not change Subswap daemon AutoSwap rules.

## Requirements

- Subswap `1.16.6` or newer with `subswap auth` (including `auth --renew` for near-expiry renewal).
- Pi `1.5.0` (verified) or a compatible version that exposes native provider registration and `lazyStream`.

## Install

Install dependencies first (this directory is not published to a package registry), then load the extension:

```bash
cd integrations/pi
npm install --ignore-scripts
```

From a Subswap checkout:

```bash
pi install ./integrations/pi
```

Or load it from the auto-discovered extension directory:

```bash
ln -s "$PWD/integrations/pi" ~/.pi/agent/extensions/subswap-pi
```

Then start a new Pi session or run `/reload`.

Set a pooled provider as the model source, for example:

```json
{
  "defaultProvider": "subswap-volcengine",
  "defaultModel": "kimi-k3.1-pro",
  "enabledModels": [
    "subswap-codex/gpt-6.1-sol",
    "subswap-volcengine/kimi-k3.1-pro"
  ]
}
```

Do not run another account-failover extension for the same providers. This extension registers `subswap-codex` and `subswap-volcengine`; aliases such as `codex-1` / `codex-2` or `coding-plan` / `coding-plan-2` are not required.

## Credentials

The extension asks Subswap for one account's current request credential through `subswap auth <provider>/<id>`. That command returns only an access token or API key:

- `kind: "credential"` for Codex access tokens, with optional expiry.
- `kind: "environment"` for Volcengine, containing `ANTHROPIC_AUTH_TOKEN` and model configuration.

It never returns a refresh token and never changes the native active account. A near-expiry credential is renewed with `subswap auth <provider>/<id> --renew`, which only renews that one account (parked accounts via the existing direct OAuth path, active Codex accounts only via official app-server coordination) and never runs quota queries, the default entry, AutoSwap, or the daemon. Renewal requests are deduplicated per account while in flight and throttled to one attempt per account per 90 seconds; unreadable command output surfaces as a generic error without secret content.

The extension caches an API key briefly and a Codex access token until shortly before expiry; a failed attempt invalidates that account's cache entry before a later retry.

Never print `subswap auth` output in logs or share it. It is a secret-bearing local IPC response.

## Verify

```bash
cd integrations/pi
npm install --ignore-scripts
npm run check
npm test
```

Live load check (with the extension installed via the route above):

```bash
pi --provider subswap-volcengine --model kimi-k3.1-pro -p "Reply with exactly: OK"
```

If every account in the group is genuinely exhausted, Pi reports the upstream quota error. That is not a failover failure; the extension only rotates when another account can be tried. Pi's own agent-level retry/backoff may delay that final error; the extension intentionally adds no progress UI, so adjust Pi retry settings if a faster failure is preferred.

## Limitations

- Cross-provider fallback is not implemented. A Codex pool stays on Codex; a Volcengine pool stays on Volcengine.
- Cursor, OpenCode, Claude OAuth, Kimi, and Command Code are not exposed by this version. They either use a different credential shape, a SQLite/GUI lifecycle, or a protocol that needs a separate adapter.
- Accounts added or removed while Pi is running are picked up after `/reload` or a new session. A removed account fails its next setup gracefully through the per-account cooldown path; it never crashes the request stream.
- The Codex pool exposes Pi's native Codex catalog (8 models on Pi 1.5.0); the count follows Pi releases.
- The Volcengine model base URL and model ID are fixed in this version. The extension uses the OpenAI-compatible path `https://ark.cn-beijing.volces.com/api/coding/v3`; the native Claude endpoint (`.../api/coding`, without `/v3`) is a different protocol path. Update `src/providers.ts` when the official endpoint or catalog changes.

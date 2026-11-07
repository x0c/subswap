# Subswap Pi integration

Pi providers backed by Subswap's account pools. Each Subswap provider becomes one Pi provider, so all accounts in that group share the same models and fail over inside a single request.

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
- Repeated attempts are bounded to once per account per request. Failed accounts cool down using `Retry-After` when present, otherwise a conservative default.
- Manually selectable Subswap API-key accounts, including `manual_only` accounts such as Volcengine Coding Plan keys, participate in request-scoped failover. This does not change Subswap daemon AutoSwap rules.

## Requirements

- Subswap `1.16.6` or newer with `subswap auth`.
- Pi `1.5.0` or a compatible version that exposes native provider registration and `lazyStream`.

## Install

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

It never returns a refresh token and never changes the native active account. The extension caches an API key briefly and a Codex access token until shortly before expiry; a failed attempt invalidates that account's cache entry before a later retry.

Never print `subswap auth` output in logs or share it. It is a secret-bearing local IPC response.

## Verify

```bash
cd integrations/pi
npm install --ignore-scripts
npm run check
npm test
```

Live load check:

```bash
pi -e ./integrations/pi --provider subswap-volcengine --model kimi-k3.1-pro -p "Reply with exactly: OK"
```

If every account in the group is genuinely exhausted, Pi reports the upstream quota error. That is not a failover failure; the extension only rotates when another account can be tried.

## Limitations

- Cross-provider fallback is not implemented. A Codex pool stays on Codex; a Volcengine pool stays on Volcengine.
- Cursor, OpenCode, Claude OAuth, Kimi, and Command Code are not exposed by this version. They either use a different credential shape, a SQLite/GUI lifecycle, or a protocol that needs a separate adapter.
- Accounts added while Pi is running are picked up after `/reload` or a new session.
- The Volcengine model base URL and model ID are fixed in this version. Update `src/providers.ts` when the official endpoint or catalog changes.

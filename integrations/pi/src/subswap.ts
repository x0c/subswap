import { execFile } from "node:child_process";

export interface SubswapAccount {
	id: string;
	provider: string;
	displayLabel: string;
	active: boolean;
	manualOnly: boolean;
	/** Account channel: accounts in one channel share one Pi provider and fail over together. */
	channel?: string;
	/** Upstream endpoint for custom channels (new status JSON; absent on old JSON). */
	endpoint?: string;
	/** Upstream model for custom channels (new status JSON; absent on old JSON). */
	model?: string;
	/** Custom-channel wire protocol, e.g. Codex `responses` (new status JSON). */
	wireApi?: string;
}

export interface RequestSecret {
	apiKey: string;
	expiresAtMs?: number;
	/** Upstream endpoint carried by the projection (environment BASE_URL or credential endpoint). */
	endpoint?: string;
	/** Upstream model carried by the projection (environment MODEL or credential model). */
	model?: string;
}

export interface ExecResult {
	stdout: string;
}

export interface ExecOptions {
	signal?: AbortSignal;
	timeoutMs: number;
	maxBuffer?: number;
}

export type Exec = (args: string[], options: ExecOptions) => Promise<ExecResult>;

interface StatusPayload {
	accounts?: Array<{
		id?: unknown;
		provider?: unknown;
		display_label?: unknown;
		active?: unknown;
		manual_only?: unknown;
		channel?: unknown;
		endpoint?: unknown;
		base_url?: unknown;
		model?: unknown;
		wire_api?: unknown;
		wireApi?: unknown;
	}>;
}

interface AuthPayload {
	kind?: unknown;
	secret?: unknown;
	expires_at_ms?: unknown;
	env?: unknown;
	endpoint?: unknown;
	base_url?: unknown;
	model?: unknown;
}

function stringField(value: unknown, fallback = ""): string {
	return typeof value === "string" ? value : fallback;
}

/** Parse subprocess stdout as a JSON object; never echo the raw text. */
function parseJsonPayload(stdout: string): Record<string, unknown> {
	let parsed: unknown;
	try {
		parsed = JSON.parse(stdout);
	} catch {
		throw new Error("Subswap returned unreadable output");
	}
	if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
		throw new Error("Subswap returned unreadable output");
	}
	return parsed as Record<string, unknown>;
}

/** Default channel for Codex OAuth accounts; keeps the `subswap-codex` Pi provider id. */
export const DEFAULT_CODEX_CHANNEL = "codex";

/** Volcengine OpenAI-compatible base URL used by the Pi provider (old-JSON fallback). */
export const VOLCENGINE_BASE_URL = "https://ark.cn-beijing.volces.com/api/coding/v3";

/** Volcengine Pi model id (old-JSON fallback). */
export const VOLCENGINE_MODEL = "kimi-k3.1-pro";

/** Default channel for standard Volcengine keys; keeps the `subswap-volcengine` Pi provider id. */
export const DEFAULT_VOLCENGINE_CHANNEL = `volcengine:${VOLCENGINE_BASE_URL}#${VOLCENGINE_MODEL}`;

/**
 * Local channel fallback when the status JSON carries no server `channel`.
 * Codex custom API channels are per account (`codex:<id>`); a Codex account is
 * custom when it carries endpoint metadata or is manual-only (only `add-api`
 * custom channels set manual-only; OAuth imports never do). Volcengine keys
 * group by endpoint and model. Anything else keeps a provider-derived channel
 * so unknown providers are preserved, never dropped.
 */
export function fallbackChannel(account: Pick<SubswapAccount, "provider" | "id" | "endpoint" | "model" | "wireApi"> & { manualOnly?: boolean }): string {
	const endpoint = account.endpoint?.replace(/\/+$/, "") || undefined;
	const model = account.model?.trim() || undefined;
	if (account.provider === "codex") {
		if (endpoint || model || account.wireApi || account.manualOnly) return `codex:${account.id}`;
		return DEFAULT_CODEX_CHANNEL;
	}
	if (account.provider === "volcengine") {
		return `volcengine:${endpoint ?? VOLCENGINE_BASE_URL}#${model ?? VOLCENGINE_MODEL}`;
	}
	if (endpoint || model) return `${account.provider}:${endpoint ?? ""}#${model ?? ""}`;
	return account.provider;
}

/** Effective channel: server value when present, local fallback otherwise. */
export function channelOf(account: SubswapAccount): string {
	const raw = account.channel?.trim();
	return raw ? raw : fallbackChannel(account);
}

export function parseStatus(payload: StatusPayload): SubswapAccount[] {
	if (!payload || typeof payload !== "object" || !Array.isArray(payload.accounts)) return [];
	return payload.accounts.flatMap((entry) => {
		const id = stringField(entry.id);
		const provider = stringField(entry.provider);
		if (!id || !provider) return [];
		const manualOnly = entry.manual_only === true;
		const endpoint = stringField(entry.endpoint) || stringField(entry.base_url) || undefined;
		const model = stringField(entry.model) || undefined;
		const wireApi = stringField(entry.wireApi) || stringField(entry.wire_api) || undefined;
		const serverChannel = stringField(entry.channel) || undefined;
		const account: SubswapAccount = {
			id,
			provider,
			displayLabel: stringField(entry.display_label, id),
			active: entry.active === true,
			manualOnly,
			...(endpoint ? { endpoint } : {}),
			...(model ? { model } : {}),
			...(wireApi ? { wireApi } : {}),
		};
		account.channel = serverChannel ?? fallbackChannel({ ...account, manualOnly });
		return [account];
	});
}

export function parseAuth(payload: AuthPayload): RequestSecret {
	const credentialEndpoint = stringField(payload.endpoint) || stringField(payload.base_url) || undefined;
	const credentialModel = stringField(payload.model) || undefined;
	if (payload.kind === "credential" && typeof payload.secret === "string" && payload.secret) {
		return {
			apiKey: payload.secret,
			...(typeof payload.expires_at_ms === "number" ? { expiresAtMs: payload.expires_at_ms } : {}),
			...(credentialEndpoint ? { endpoint: credentialEndpoint } : {}),
			...(credentialModel ? { model: credentialModel } : {}),
		};
	}
	if (payload.kind === "environment" && payload.env && typeof payload.env === "object") {
		const env = payload.env as Record<string, unknown>;
		const apiKey = [env.ANTHROPIC_AUTH_TOKEN, env.ANTHROPIC_API_KEY]
			.find((value): value is string => typeof value === "string" && value.length > 0);
		if (apiKey) {
			const endpoint = stringField(env.ANTHROPIC_BASE_URL) || credentialEndpoint;
			const model = stringField(env.ANTHROPIC_MODEL)
				|| stringField(env.ANTHROPIC_DEFAULT_SONNET_MODEL)
				|| credentialModel;
			return {
				apiKey,
				...(endpoint ? { endpoint } : {}),
				...(model ? { model } : {}),
			};
		}
	}
	throw new Error("Subswap returned no usable request credential");
}

function defaultExec(binary: string): Exec {
	return (args, options) => new Promise((resolve, reject) => {
		execFile(binary, args, {
			timeout: options.timeoutMs,
			maxBuffer: options.maxBuffer ?? 2 * 1024 * 1024,
			signal: options.signal,
			windowsHide: true,
		}, (error, stdout, stderr) => {
			if (error) {
				// stderr/stdout can contain sensitive provider details; never copy them into Pi logs.
				reject(new Error(`Subswap ${args[0] ?? "command"} failed`));
				return;
			}
			resolve({ stdout });
		});
	});
}

export class SubswapClient {
	readonly #exec: Exec;
	#renewals = new Map<string, Promise<void>>();
	#lastRenewalAttempt = new Map<string, number>();
	#cache = new Map<string, { secret: RequestSecret; usableUntil: number }>();

	constructor(options: { binary?: string; exec?: Exec } = {}) {
		this.#exec = options.exec ?? defaultExec(options.binary ?? process.env.SUBSWAP_BIN ?? "subswap");
	}

	async accounts(signal?: AbortSignal): Promise<SubswapAccount[]> {
		const result = await this.#exec(["status", "--json", "--no-metrics"], {
			signal,
			timeoutMs: 5_000,
			maxBuffer: 2 * 1024 * 1024,
		});
		return parseStatus(parseJsonPayload(result.stdout) as StatusPayload);
	}

	async #projection(account: SubswapAccount, signal?: AbortSignal): Promise<RequestSecret> {
		const result = await this.#exec(["auth", `${account.provider}/${account.id}`], {
			signal,
			timeoutMs: 10_000,
			maxBuffer: 1024 * 1024,
		});
		return parseAuth(parseJsonPayload(result.stdout) as AuthPayload);
	}

	async requestSecret(account: SubswapAccount, signal?: AbortSignal): Promise<RequestSecret> {
		const cacheKey = `${account.provider}/${account.id}`;
		const cached = this.#cache.get(cacheKey);
		if (cached && cached.usableUntil > Date.now()) return cached.secret;

		let secret = await this.#projection(account, signal);
		if (secret.expiresAtMs !== undefined && secret.expiresAtMs <= Date.now() + 60_000) {
			await this.renew(account, signal);
			secret = await this.#projection(account, signal);
			if (secret.expiresAtMs !== undefined && secret.expiresAtMs <= Date.now()) {
				throw new Error(`Subswap account ${account.displayLabel} needs a fresh login`);
			}
		}
		const usableUntil = secret.expiresAtMs === undefined
			? Date.now() + 60_000
			: secret.expiresAtMs - 60_000;
		this.#cache.set(cacheKey, { secret, usableUntil });
		return secret;
	}

	invalidate(account: SubswapAccount): void {
		this.#cache.delete(`${account.provider}/${account.id}`);
	}

	async renew(account: SubswapAccount, signal?: AbortSignal): Promise<void> {
		const key = `${account.provider}/${account.id}`;
		const inflight = this.#renewals.get(key);
		if (inflight) return inflight;
		if (Date.now() - (this.#lastRenewalAttempt.get(key) ?? 0) < 90_000) return;
		this.#lastRenewalAttempt.set(key, Date.now());
		const renewal = this.#exec(["auth", key, "--renew"], {
			signal,
			timeoutMs: 45_000,
			maxBuffer: 8 * 1024 * 1024,
		}).then(() => undefined).finally(() => {
			this.#renewals.delete(key);
		});
		this.#renewals.set(key, renewal);
		return renewal;
	}
}

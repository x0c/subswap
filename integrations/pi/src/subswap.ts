import { execFile } from "node:child_process";

export interface SubswapAccount {
	id: string;
	provider: string;
	displayLabel: string;
	active: boolean;
	manualOnly: boolean;
}

export interface RequestSecret {
	apiKey: string;
	expiresAtMs?: number;
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
	}>;
}

interface AuthPayload {
	kind?: unknown;
	secret?: unknown;
	expires_at_ms?: unknown;
	env?: unknown;
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

export function parseStatus(payload: StatusPayload): SubswapAccount[] {
	if (!payload || typeof payload !== "object" || !Array.isArray(payload.accounts)) return [];
	return payload.accounts.flatMap((entry) => {
		const id = stringField(entry.id);
		const provider = stringField(entry.provider);
		if (!id || !provider) return [];
		return [{
			id,
			provider,
			displayLabel: stringField(entry.display_label, id),
			active: entry.active === true,
			manualOnly: entry.manual_only === true,
		}];
	});
}

export function parseAuth(payload: AuthPayload): RequestSecret {
	if (payload.kind === "credential" && typeof payload.secret === "string" && payload.secret) {
		return {
			apiKey: payload.secret,
			...(typeof payload.expires_at_ms === "number" ? { expiresAtMs: payload.expires_at_ms } : {}),
		};
	}
	if (payload.kind === "environment" && payload.env && typeof payload.env === "object") {
		const env = payload.env as Record<string, unknown>;
		const apiKey = [env.ANTHROPIC_AUTH_TOKEN, env.ANTHROPIC_API_KEY]
			.find((value): value is string => typeof value === "string" && value.length > 0);
		if (apiKey) return { apiKey };
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

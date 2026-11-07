import {
	lazyStream,
	type Api,
	type AssistantMessageEvent,
	type AssistantMessageEventStream,
	type Model,
	type Provider,
	type ProviderResponse,
	type SimpleStreamOptions,
	type StreamOptions,
	type TranscriptContext,
} from "@earendil-works/pi-ai";
import type { RequestSecret, SubswapAccount } from "./subswap.ts";

type RequestOptions = SimpleStreamOptions & Record<string, unknown>;
type StreamKind = "stream" | "streamSimple";

export interface PoolState {
	lastHealthyId?: string;
	cooldowns: Map<string, number>;
}

export interface FailoverNotice {
	provider: string;
	from: string;
	to: string;
	reason: string;
}

export interface PoolHooks {
	onFailover?: (notice: FailoverNotice) => void;
	onAccountFailure?: (account: SubswapAccount) => void;
}

interface Terminal {
	start?: AssistantMessageEvent & { type: "start" };
	event: AssistantMessageEvent & { type: "error" };
}

const DEFAULT_COOLDOWN_MS = 5 * 60_000;
const AUTH_COOLDOWN_MS = 60_000;
const MAX_COOLDOWN_MS = 6 * 60 * 60_000;

export function newPoolState(): PoolState {
	return { cooldowns: new Map() };
}

export function classifyAccountFailure(message: string, status?: number): boolean {
	if (status === 401 || status === 402 || status === 429) return true;
	if (status === 403) {
		return /quota|credit|billing|balance|rate.?limit|unauthorized|forbidden|invalid.*(?:key|token)|authentication/i.test(message);
	}
	if (status !== undefined) return false;
	return /\b(?:401|402|429)\b|usage_limit_reached|accountquotaexceeded|quota.?exceed|insufficient.*(?:credit|quota|balance)|rate.?limit|invalid.*(?:api.?key|token)|authentication_error|unauthorized/i.test(message);
}

export function cooldownFor(message: string, status: number | undefined, headers: Record<string, string> | undefined, now: number): number {
	const base = status === 401 || status === 403 ? AUTH_COOLDOWN_MS : DEFAULT_COOLDOWN_MS;
	const value = headers?.["retry-after"];
	if (!value) return now + base;
	const seconds = Number(value);
	const retryAt = Number.isFinite(seconds) && seconds >= 0
		? now + Math.min(seconds * 1000, MAX_COOLDOWN_MS)
		: Date.parse(value);
	return Number.isFinite(retryAt) ? Math.max(now + base, Math.min(retryAt, now + MAX_COOLDOWN_MS)) : now + base;
}

export function orderedAccounts(accounts: SubswapAccount[], state: PoolState, attempted: ReadonlySet<string>, now: number): SubswapAccount[] {
	const remaining = accounts.filter((account) => !attempted.has(account.id));
	const available = remaining.filter((account) => (state.cooldowns.get(account.id) ?? 0) <= now);
	const candidates = available.length > 0 ? available : remaining;
	return candidates.sort((a, b) => {
		const healthyA = Number(a.id === state.lastHealthyId);
		const healthyB = Number(b.id === state.lastHealthyId);
		if (healthyA !== healthyB) return healthyB - healthyA;
		if (a.active !== b.active) return Number(b.active) - Number(a.active);
		return (state.cooldowns.get(a.id) ?? 0) - (state.cooldowns.get(b.id) ?? 0);
	});
}

function callProvider(
	provider: Provider<Api>,
	kind: StreamKind,
	model: Model<Api>,
	context: TranscriptContext,
	options: RequestOptions,
): AssistantMessageEventStream {
	return kind === "streamSimple"
		? provider.streamSimple(model, context, options)
		: provider.stream(model, context, options);
}

function isAbort(event: AssistantMessageEvent & { type: "error" }, signal: AbortSignal): boolean {
	return event.reason === "aborted" || signal.aborted;
}

function pooledStream(
	provider: Provider<Api>,
	accounts: SubswapAccount[],
	state: PoolState,
	resolveSecret: (account: SubswapAccount, signal: AbortSignal) => Promise<RequestSecret>,
	kind: StreamKind,
	model: Model<Api>,
	context: TranscriptContext,
	options: RequestOptions | undefined,
	hooks: PoolHooks,
): AssistantMessageEventStream {
	return lazyStream(model, async () => {
		const attempted = new Set<string>();
		const requestOptions = { ...(options ?? {}) } as RequestOptions;
		const signal = requestOptions.signal ?? new AbortController().signal;
		let lastRejected: Terminal | undefined;
		let lastSetupError: unknown;

		return (async function* (): AsyncGenerator<AssistantMessageEvent> {
			while (true) {
				const candidates = orderedAccounts(accounts, state, attempted, Date.now());
				const account = candidates[0];
				if (!account) {
					if (lastRejected) {
						if (lastRejected.start) yield lastRejected.start;
						yield lastRejected.event;
						return;
					}
					throw lastSetupError ?? new Error(`Subswap provider ${provider.id} has no usable accounts`);
				}
				attempted.add(account.id);
				let secret: RequestSecret;
				try {
					secret = await resolveSecret(account, signal);
				} catch (error) {
					lastSetupError = error;
					state.cooldowns.set(account.id, Date.now() + AUTH_COOLDOWN_MS);
					continue;
				}

				const attemptOptions = {
					...requestOptions,
					apiKey: secret.apiKey,
					maxRetries: 0,
				} as RequestOptions;
				const onResponse = attemptOptions.onResponse;
				let response: Pick<ProviderResponse, "status" | "headers"> | undefined;
				attemptOptions.onResponse = async (nextResponse, responseModel) => {
					response = { status: nextResponse.status, headers: { ...nextResponse.headers } };
					await onResponse?.(nextResponse, responseModel);
				};

				let start: Terminal["start"];
				let outputStarted = false;
				let rotate = false;
				let terminalSeen = false;
				const inner = callProvider(provider, kind, model, context, attemptOptions);
				for await (const event of inner) {
					if (event.type === "start") {
						start = event;
						continue;
					}
					if (event.type === "error") {
						terminalSeen = true;
						const errorText = event.error.errorMessage ?? "Provider request failed";
						if (!isAbort(event, signal) && !outputStarted && classifyAccountFailure(errorText, response?.status)) {
							state.cooldowns.set(account.id, cooldownFor(errorText, response?.status, response?.headers, Date.now()));
							hooks.onAccountFailure?.(account);
							if (orderedAccounts(accounts, state, attempted, Date.now()).length > 0) {
								lastRejected = { ...(start ? { start } : {}), event };
								const next = orderedAccounts(accounts, state, attempted, Date.now())[0];
								if (next) {
									hooks.onFailover?.({
										provider: provider.id,
										from: account.displayLabel,
										to: next.displayLabel,
										reason: response?.status ? `HTTP ${response.status}` : errorText,
									});
								}
								rotate = true;
								break;
							}
						}
						if (!outputStarted && start) yield start;
						yield event;
						return;
					}
					if (event.type === "done") {
						terminalSeen = true;
						state.lastHealthyId = account.id;
						state.cooldowns.delete(account.id);
						if (!outputStarted && start) yield start;
						yield event;
						return;
					}
					if (!outputStarted) {
						outputStarted = true;
						if (start) yield start;
					}
					yield event;
				}
				if (rotate) continue;
				if (!terminalSeen) throw new Error(`Subswap provider ${provider.id} stream ended without a terminal event`);
			}
		})();
	});
}

export function poolProvider(
	base: Provider<Api>,
	poolId: string,
	displayName: string,
	accounts: SubswapAccount[],
	state: PoolState,
	resolveSecret: (account: SubswapAccount, signal: AbortSignal) => Promise<RequestSecret>,
	hooks: PoolHooks = {},
): Provider<Api> {
	const models = base.getModels().map((model) => ({
		...model,
		provider: poolId,
		name: `${model.name} (Subswap)`,
	}));
	const auth = {
		apiKey: {
			name: `${displayName} managed by Subswap`,
			async check() {
				return accounts.length > 0 ? { type: "oauth" as const, source: "subswap" } : undefined;
			},
			async resolve({ signal }: { signal: AbortSignal }) {
				const account = orderedAccounts(accounts, state, new Set(), Date.now())[0];
				if (!account) return undefined;
				const secret = await resolveSecret(account, signal);
				return { auth: { apiKey: secret.apiKey }, source: "subswap" };
			},
		},
	};
	return {
		...base,
		id: poolId,
		name: displayName,
		auth,
		getModels: () => models,
		getAllModels: () => models,
		stream(model, context, options) {
			return pooledStream(base, accounts, state, resolveSecret, "stream", model, context, options as RequestOptions, hooks);
		},
		streamSimple(model, context, options) {
			return pooledStream(base, accounts, state, resolveSecret, "streamSimple", model, context, options as RequestOptions, hooks);
		},
	};
}

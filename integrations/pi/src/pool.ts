import {
	lazyStream,
	type Api,
	type AssistantMessage,
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
import { channelOf } from "./subswap.ts";

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
	/**
	 * Non-native pools only: derive the per-attempt request model from the
	 * account and its live secret (custom channels may carry per-account
	 * endpoint/model). Native pools leave the request model untouched.
	 */
	resolveModel?: (account: SubswapAccount, secret: RequestSecret, model: Model<Api>) => Model<Api>;
}

interface Terminal {
	start?: AssistantMessageEvent & { type: "start" };
	event: AssistantMessageEvent & { type: "error" };
}

// 本轮级联中最近一次的失败：HTTP 拒绝与凭证 setup 失败互相覆盖，耗尽时只呈现最近者。
type LastFailure =
	| { kind: "http"; start?: Terminal["start"]; event: Terminal["event"] }
	| { kind: "setup"; error: Error };

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
	// 2xx 只表示流已建立，额度拒绝仍可能来自流内；明确的其它 HTTP 错误不按文字误切。
	if (status !== undefined && (status < 200 || status >= 300)) return false;
	return /\b(?:401|402|429)\b|\busage[ _]limit[ _](?:has[ _]been[ _])?(?:reached|exceeded)\b|accountquotaexceeded|quota.?exceed|insufficient.*(?:credit|quota|balance)|rate.?limit|invalid.*(?:api.?key|token)|authentication_error|unauthorized/i.test(message);
}

export function cooldownFor(message: string, status: number | undefined, headers: Record<string, string> | undefined, now: number): number {
	const base = status === 401 || status === 403 ? AUTH_COOLDOWN_MS : DEFAULT_COOLDOWN_MS;
	const value = headerValue(headers, "retry-after");
	if (!value) return now + base;
	const seconds = Number(value);
	const retryAt = Number.isFinite(seconds) && seconds >= 0
		? now + Math.min(seconds * 1000, MAX_COOLDOWN_MS)
		: Date.parse(value);
	return Number.isFinite(retryAt) ? Math.max(now + base, Math.min(retryAt, now + MAX_COOLDOWN_MS)) : now + base;
}

export function orderedAccounts(accounts: SubswapAccount[], state: PoolState, attempted: ReadonlySet<string>, now: number, channel?: string): SubswapAccount[] {
	const inChannel = channel === undefined
		? accounts
		: accounts.filter((account) => channelOf(account) === channel);
	const remaining = inChannel.filter((account) => !attempted.has(account.id));
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

// ASCII 大小写不敏感的响应头查找：fetch Headers 本来就是小写，但不依赖调用方。
function headerValue(headers: Record<string, string> | undefined, name: string): string | undefined {
	if (!headers) return undefined;
	for (const key of Object.keys(headers)) {
		if (key.toLowerCase() === name) return headers[key];
	}
	return undefined;
}

// 只保留 message：setup 错误来自外部 resolveSecret，避免把非 message 字段带入 Pi 错误面。
function sanitizedSetupError(error: unknown): Error {
	return error instanceof Error ? new Error(error.message) : new Error(String(error));
}

// 合成原生形态的取消事件：调用方在 start 之前取消时，原生适配器同样直接以 error/aborted 结束。
function abortedEvent(model: Model<Api>): AssistantMessageEvent & { type: "error" } {
	const aborted: AssistantMessage = {
		role: "assistant",
		content: [],
		api: model.api,
		provider: model.provider,
		model: model.id,
		usage: {
			input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "aborted",
		timestamp: Date.now(),
	};
	return { type: "error", reason: "aborted", error: aborted };
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
		// One pool serves exactly one channel; mixed channels are a wiring bug,
		// never a failover path. Fail closed before any credential is resolved.
		const poolChannel = accounts.length > 0
			? channelOf(accounts[0] as SubswapAccount)
			: undefined;
		if (poolChannel !== undefined) {
			const mixed = accounts.some((account) => channelOf(account) !== poolChannel);
			if (mixed) throw new Error(`Subswap provider ${provider.id} mixes channels; refusing to stream`);
		}
		const requestOptions = { ...(options ?? {}) } as RequestOptions;
		const signal = requestOptions.signal ?? new AbortController().signal;
		let lastFailure: LastFailure | undefined;
		// 本轮写入的冷却（id 与写入值）：取消时回滚，取消不留痕；被并发请求覆盖的值不动。
		const cooledThisRequest = new Map<string, number>();
		const setCooldown = (id: string, until: number): void => {
			state.cooldowns.set(id, until);
			cooledThisRequest.set(id, until);
		};
		const rollbackCooldowns = (): void => {
			for (const [id, until] of cooledThisRequest) {
				if (state.cooldowns.get(id) === until) state.cooldowns.delete(id);
			}
			cooledThisRequest.clear();
		};

		return (async function* (): AsyncGenerator<AssistantMessageEvent> {
			while (true) {
				// 调用方已取消：立即停，不再试其它账号，不调失败/切换 hook。
				if (signal.aborted) {
					rollbackCooldowns();
					yield abortedEvent(model);
					return;
				}
				const candidates = orderedAccounts(accounts, state, attempted, Date.now(), poolChannel);
				const account = candidates[0];
				if (!account) {
					if (lastFailure?.kind === "http") {
						if (lastFailure.start) yield lastFailure.start;
						yield lastFailure.event;
						return;
					}
					throw lastFailure?.kind === "setup"
						? lastFailure.error
						: new Error(`Subswap provider ${provider.id} has no usable accounts`);
				}
				attempted.add(account.id);
				let secret: RequestSecret;
				try {
					secret = await resolveSecret(account, signal);
				} catch (error) {
					// 解析凭证时被取消：按取消处理，不记冷却、不调 hook。
					if (signal.aborted) {
						rollbackCooldowns();
						yield abortedEvent(model);
						return;
					}
					lastFailure = { kind: "setup", error: sanitizedSetupError(error) };
					setCooldown(account.id, Date.now() + AUTH_COOLDOWN_MS);
					continue;
				}

				const attemptOptions = {
					...requestOptions,
					apiKey: secret.apiKey,
					maxRetries: 0,
				} as RequestOptions;
				// Custom channels may resolve a per-account endpoint/model; the
				// native branch keeps the request model untouched.
				const attemptModel = hooks.resolveModel?.(account, secret, model) ?? model;
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
				const inner = callProvider(provider, kind, attemptModel, context, attemptOptions);
				for await (const event of inner) {
					if (event.type === "start") {
						start = event;
						continue;
					}
					if (event.type === "error") {
						terminalSeen = true;
						const errorText = event.error.errorMessage ?? "Provider request failed";
						if (!isAbort(event, signal) && !outputStarted && classifyAccountFailure(errorText, response?.status)) {
							setCooldown(account.id, cooldownFor(errorText, response?.status, response?.headers, Date.now()));
							hooks.onAccountFailure?.(account);
							if (orderedAccounts(accounts, state, attempted, Date.now(), poolChannel).length > 0) {
								lastFailure = { ...(start ? { start } : {}), event, kind: "http" };
								const next = orderedAccounts(accounts, state, attempted, Date.now(), poolChannel)[0];
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

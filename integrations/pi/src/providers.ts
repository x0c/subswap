import {
	createProvider,
	type Api,
	type Model,
	type Provider,
} from "@earendil-works/pi-ai";
import { openAICompletionsApi, openAIResponsesApi } from "@earendil-works/pi-ai/compat";
import { builtinProviders } from "@earendil-works/pi-ai/providers/all";
import { poolProvider, type FailoverNotice, type PoolState } from "./pool.ts";
import {
	channelOf,
	DEFAULT_CODEX_CHANNEL,
	DEFAULT_VOLCENGINE_CHANNEL,
	VOLCENGINE_BASE_URL,
	VOLCENGINE_MODEL,
	type RequestSecret,
	type SubswapAccount,
} from "./subswap.ts";

export { DEFAULT_CODEX_CHANNEL, DEFAULT_VOLCENGINE_CHANNEL, VOLCENGINE_BASE_URL, VOLCENGINE_MODEL };

export interface PoolRuntime {
	channel: string;
	providerId: string;
	accounts: SubswapAccount[];
	state: PoolState;
}

export interface PoolBuildOptions {
	channel: string;
	accounts: SubswapAccount[];
	state: PoolState;
	resolveSecret: (account: SubswapAccount, signal: AbortSignal) => Promise<RequestSecret>;
	onFailover?: (notice: FailoverNotice) => void;
	onAccountFailure?: (account: SubswapAccount) => void;
}

/** Pi-servable channel families. Anything else is discovered but has no Pi adapter. */
export type ChannelKind = "codex-oauth" | "codex-custom" | "volcengine" | "unsupported";

export function channelKind(channel: string): ChannelKind {
	if (channel === DEFAULT_CODEX_CHANNEL) return "codex-oauth";
	if (channel === DEFAULT_VOLCENGINE_CHANNEL || channel.startsWith("volcengine:")) return "volcengine";
	if (channel.startsWith("codex:")) return "codex-custom";
	return "unsupported";
}

/** One channel maps to one Pi provider id; defaults keep their legacy ids. */
export function providerIdForChannel(channel: string): string {
	if (channel === DEFAULT_CODEX_CHANNEL) return "subswap-codex";
	if (channel === DEFAULT_VOLCENGINE_CHANNEL) return "subswap-volcengine";
	return `subswap-${slugifyChannel(channel)}`;
}

function slugifyChannel(channel: string): string {
	const slug = channel
		.toLowerCase()
		.replace(/[^a-z0-9]+/g, "-")
		.replace(/-+/g, "-")
		.replace(/^-|-$/g, "");
	return slug || "custom";
}

function zeroCost(): Model<Api>["cost"] {
	return { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
}

function volcengineEndpoint(raw: string | undefined): string {
	const trimmed = (raw ?? "").trim().replace(/\/+$/, "");
	if (!trimmed) return VOLCENGINE_BASE_URL;
	// The Anthropic-compatible base (`.../api/coding`) maps to the
	// OpenAI-compatible Coding Plan path (`.../api/coding/v3`) this pool serves.
	if (trimmed.endsWith("/api/coding")) return `${trimmed}/v3`;
	return trimmed;
}

function volcengineModelFor(providerId: string, endpoint: string, modelId: string): Model<"openai-completions"> {
	return {
		id: modelId,
		name: modelId === VOLCENGINE_MODEL ? "Kimi K3.1 Pro" : modelId,
		api: "openai-completions",
		provider: providerId,
		baseUrl: endpoint,
		reasoning: true,
		thinkingLevelMap: { xhigh: "xhigh", max: "max" },
		input: ["text", "image"],
		cost: zeroCost(),
		contextWindow: 1_024_000,
		maxTokens: 1_024_000,
	};
}

function codexCustomModelFor(providerId: string, endpoint: string, modelId: string): Model<"openai-responses"> {
	return {
		id: modelId,
		name: modelId,
		api: "openai-responses",
		provider: providerId,
		baseUrl: endpoint,
		reasoning: true,
		thinkingLevelMap: { xhigh: "xhigh", max: "max" },
		input: ["text"],
		cost: zeroCost(),
		// Unknown custom endpoints: generous passthrough display values; real
		// context errors surface from upstream instead of rotating the pool.
		contextWindow: 1_024_000,
		maxTokens: 32_768,
	};
}

/**
 * Volcengine per-attempt model: endpoint may refresh from the live secret
 * (normalized to the OpenAI-compatible path); the Pi model id only follows
 * status metadata, never the Anthropic-namespace `ANTHROPIC_MODEL`, so an old
 * status JSON keeps serving the verified `kimi-k3.1-pro` id.
 */
function resolveVolcengineModel(account: SubswapAccount, secret: RequestSecret, model: Model<Api>): Model<Api> {
	const endpoint = volcengineEndpoint(account.endpoint ?? secret.endpoint ?? model.baseUrl);
	const id = account.model?.trim() || model.id;
	if (endpoint === model.baseUrl && id === model.id) return model;
	return { ...model, baseUrl: endpoint, id };
}

/** Codex custom per-attempt model: endpoint and model follow account, then live secret. */
function resolveCodexCustomModel(account: SubswapAccount, secret: RequestSecret, model: Model<Api>): Model<Api> {
	const raw = (account.endpoint ?? secret.endpoint ?? model.baseUrl).trim().replace(/\/+$/, "");
	const endpoint = raw || model.baseUrl;
	const id = account.model?.trim() || secret.model?.trim() || model.id;
	if (endpoint === model.baseUrl && id === model.id) return model;
	return { ...model, baseUrl: endpoint, id };
}

export function buildPool(options: PoolBuildOptions): Provider<Api> {
	const first = options.accounts[0];
	if (!first) throw new Error(`Subswap ${options.channel} pool is empty`);
	const providerId = providerIdForChannel(options.channel);
	const hooks = {
		onFailover: options.onFailover,
		onAccountFailure: options.onAccountFailure,
	};
	if (channelKind(options.channel) === "codex-oauth") {
		const native = builtinProviders().find((provider) => provider.id === "openai-codex");
		if (!native) throw new Error("This Pi version does not include the native Codex provider");
		return poolProvider(
			native as Provider<Api>,
			providerId,
			"Codex accounts (Subswap)",
			options.accounts,
			options.state,
			options.resolveSecret,
			hooks,
		);
	}
	if (channelKind(options.channel) === "volcengine") {
		const endpoint = volcengineEndpoint(first.endpoint);
		const modelId = first.model?.trim() || VOLCENGINE_MODEL;
		const base = createProvider({
			id: providerId,
			name: "Volcengine Coding Plan (Subswap)",
			baseUrl: endpoint,
			auth: {
				apiKey: {
					name: "Volcengine key managed by Subswap",
					async resolve() { return undefined; },
				},
			},
			models: [volcengineModelFor(providerId, endpoint, modelId)],
			api: openAICompletionsApi(),
		});
		return poolProvider(
			base as Provider<Api>,
			providerId,
			"Volcengine Coding Plan (Subswap)",
			options.accounts,
			options.state,
			options.resolveSecret,
			{ ...hooks, resolveModel: resolveVolcengineModel },
		);
	}
	if (channelKind(options.channel) === "codex-custom") {
		const endpoint = first.endpoint?.trim().replace(/\/+$/, "");
		const modelId = first.model?.trim() || undefined;
		// Old status JSON without endpoint metadata cannot route HTTP; the
		// channel stays discovered but gets no Pi provider until Subswap emits it.
		if (!endpoint || !modelId) {
			throw new Error(`Subswap ${options.channel} has no endpoint metadata; skipping Pi provider`);
		}
		const base = createProvider({
			id: providerId,
			name: `Codex custom channel ${first.id} (Subswap)`,
			baseUrl: endpoint,
			auth: {
				apiKey: {
					name: "Codex custom key managed by Subswap",
					async resolve() { return undefined; },
				},
			},
			models: [codexCustomModelFor(providerId, endpoint, modelId)],
			api: openAIResponsesApi(),
		});
		return poolProvider(
			base as Provider<Api>,
			providerId,
			`Codex custom channel ${first.id} (Subswap)`,
			options.accounts,
			options.state,
			options.resolveSecret,
			{ ...hooks, resolveModel: resolveCodexCustomModel },
		);
	}
	throw new Error(`Subswap channel ${options.channel} has no Pi adapter`);
}

export function buildPools(
	accounts: SubswapAccount[],
	states: Map<string, PoolState>,
	resolveSecret: PoolBuildOptions["resolveSecret"],
	onFailover: PoolBuildOptions["onFailover"] = undefined,
	onAccountFailure: PoolBuildOptions["onAccountFailure"] = undefined,
): Array<{ provider: Provider<Api>; runtime: PoolRuntime }> {
	const byChannel = new Map<string, SubswapAccount[]>();
	for (const account of accounts) {
		const channel = channelOf(account);
		const group = byChannel.get(channel);
		if (group) group.push(account);
		else byChannel.set(channel, [account]);
	}
	const output: Array<{ provider: Provider<Api>; runtime: PoolRuntime }> = [];
	const seenProviderIds = new Set<string>();
	for (const [channel, poolAccounts] of byChannel) {
		if (channelKind(channel) === "unsupported") continue;
		const state = states.get(channel);
		if (!state) throw new Error(`Missing state for Subswap ${channel}`);
		let provider: Provider<Api>;
		try {
			provider = buildPool({ channel, accounts: poolAccounts, state, resolveSecret, onFailover, onAccountFailure });
		} catch (error) {
			// Unservable old-JSON custom channels stay discovered but unlisted.
			if (error instanceof Error && error.message.includes("has no endpoint metadata")) continue;
			throw error;
		}
		// Channel slugs must stay unique; keep the first pool on collision.
		if (seenProviderIds.has(provider.id)) continue;
		seenProviderIds.add(provider.id);
		output.push({
			provider,
			runtime: { channel, providerId: provider.id, accounts: poolAccounts, state },
		});
	}
	return output;
}

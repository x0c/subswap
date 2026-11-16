import {
	createProvider,
	type Api,
	type Model,
	type Provider,
} from "@earendil-works/pi-ai";
import { openAICompletionsApi, openAIResponsesApi } from "@earendil-works/pi-ai/compat";
import { builtinProviders } from "@earendil-works/pi-ai/providers/all";
import { poolProvider, type FailoverNotice, type PoolState } from "./pool.ts";
import { VOLCENGINE_MODELS, volcengineModel, volcengineModelSpec, zeroCost } from "./volcengine-models.ts";
import {
	channelOf,
	DEFAULT_CODEX_CHANNEL,
	DEFAULT_OPENCODE_GO_CHANNEL,
	DEFAULT_OPENCODE_ZEN_CHANNEL,
	DEFAULT_VOLCENGINE_CHANNEL,
	OPENCODE_GO_BASE_URL,
	OPENCODE_ZEN_BASE_URL,
	VOLCENGINE_BASE_URL,
	VOLCENGINE_MODEL,
	type RequestSecret,
	type SubswapAccount,
} from "./subswap.ts";

export {
	DEFAULT_CODEX_CHANNEL,
	DEFAULT_OPENCODE_GO_CHANNEL,
	DEFAULT_OPENCODE_ZEN_CHANNEL,
	DEFAULT_VOLCENGINE_CHANNEL,
	OPENCODE_GO_BASE_URL,
	OPENCODE_ZEN_BASE_URL,
	VOLCENGINE_BASE_URL,
	VOLCENGINE_MODEL,
};

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
export type ChannelKind =
	| "codex-oauth"
	| "codex-custom"
	| "volcengine"
	| "opencode-go"
	| "opencode-zen"
	| "unsupported";

export function channelKind(channel: string): ChannelKind {
	if (channel === DEFAULT_CODEX_CHANNEL) return "codex-oauth";
	if (channel === DEFAULT_VOLCENGINE_CHANNEL || channel.startsWith("volcengine:")) return "volcengine";
	if (channel === DEFAULT_OPENCODE_GO_CHANNEL || channel.startsWith("opencode-go:")) return "opencode-go";
	if (channel === DEFAULT_OPENCODE_ZEN_CHANNEL || channel.startsWith("opencode-zen:")) return "opencode-zen";
	if (channel.startsWith("codex:")) return "codex-custom";
	return "unsupported";
}

/** One channel maps to one Pi provider id; defaults keep their legacy ids. */
export function providerIdForChannel(channel: string): string {
	if (channel === DEFAULT_CODEX_CHANNEL) return "subswap-codex";
	if (channel === DEFAULT_VOLCENGINE_CHANNEL) return "subswap-volcengine";
	if (channel === DEFAULT_OPENCODE_GO_CHANNEL) return "subswap-opencode-go";
	if (channel === DEFAULT_OPENCODE_ZEN_CHANNEL) return "subswap-opencode-zen";
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

function volcengineEndpoint(raw: string | undefined): string {
	const trimmed = (raw ?? "").trim().replace(/\/+$/, "");
	if (!trimmed) return VOLCENGINE_BASE_URL;
	// The Anthropic-compatible base (`.../api/coding`) maps to the
	// OpenAI-compatible Coding Plan path (`.../api/coding/v3`) this pool serves.
	if (trimmed.endsWith("/api/coding")) return `${trimmed}/v3`;
	return trimmed;
}

/**
 * 非目录模型：保持旧行为（展示名默认就是原始 id，除了已验证的 `kimi-k3.1-pro`）。
 * 自定义中转端点的通道与账号声明的未知 id（如 `ark-code-latest`）仍走这条路径。
 */
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
 * Volcengine 单次尝试：账号只提供端点（账号元数据优先，其次投影端点，最后沿用模型 baseUrl，
 * 统一归一化到 OpenAI 兼容的 `/v3` 路径）；模型 id 完全由用户请求决定，
 * 不跟随账号的 `model` 字段，因此账号间故障转移始终请求同一个模型。
 */
function resolveVolcengineModel(account: SubswapAccount, secret: RequestSecret, model: Model<Api>): Model<Api> {
	const endpoint = volcengineEndpoint(account.endpoint ?? secret.endpoint ?? model.baseUrl);
	if (endpoint === model.baseUrl) return model;
	return { ...model, baseUrl: endpoint };
}

/**
 * Volcengine 通道暴露的模型清单：标准 Ark Coding Plan 端点给出完整目录，
 * 并把账号声明的非目录 id（如 `ark-code-latest`）按旧方式各追加一条；
 * 自定义中转端点的模型目录未知，只保留首个账号声明的单个模型。
 */
function volcengineChannelModels(
	providerId: string,
	endpoint: string,
	accounts: SubswapAccount[],
): Array<Model<"openai-completions">> {
	if (endpoint !== VOLCENGINE_BASE_URL) {
		const modelId = accounts[0]?.model?.trim() || VOLCENGINE_MODEL;
		return [volcengineModelFor(providerId, endpoint, modelId)];
	}
	const catalog = VOLCENGINE_MODELS.map((spec) => volcengineModel(providerId, endpoint, spec));
	const declared: Array<Model<"openai-completions">> = [];
	for (const account of accounts) {
		const modelId = account.model?.trim();
		// 目录 id 已由目录覆盖；别名/自动路由/账号自定义 id 才需要追加，且按 id 去重。
		if (!modelId || volcengineModelSpec(modelId) || declared.some((model) => model.id === modelId)) continue;
		declared.push(volcengineModelFor(providerId, endpoint, modelId));
	}
	return [...catalog, ...declared];
}

/** Codex custom per-attempt model: endpoint and model follow account, then live secret. */
function resolveCodexCustomModel(account: SubswapAccount, secret: RequestSecret, model: Model<Api>): Model<Api> {
	const raw = (account.endpoint ?? secret.endpoint ?? model.baseUrl).trim().replace(/\/+$/, "");
	const endpoint = raw || model.baseUrl;
	const id = account.model?.trim() || secret.model?.trim() || model.id;
	if (endpoint === model.baseUrl && id === model.id) return model;
	return { ...model, baseUrl: endpoint, id };
}

/**
 * OpenCode per-attempt model: keeps per-model protocol path suffixes (`/anthropic`,
 * `/google/v1beta`, `/openai/v1`) intact when `account.endpoint` equals the default root,
 * and swaps only the root prefix if a custom mirror/override endpoint is configured.
 */
function resolveOpenCodeModel(defaultRootUrl: string) {
	return (account: SubswapAccount, secret: RequestSecret, model: Model<Api>): Model<Api> => {
		const root = (account.endpoint ?? secret.endpoint ?? "").trim().replace(/\/+$/, "");
		if (!root || root === defaultRootUrl) return model;
		const nextBaseUrl = model.baseUrl.startsWith(defaultRootUrl)
			? `${root}${model.baseUrl.slice(defaultRootUrl.length)}`
			: root;
		if (nextBaseUrl === model.baseUrl) return model;
		return { ...model, baseUrl: nextBaseUrl };
	};
}

function normalizeOpenCodeBaseProvider(native: Provider<Api>): Provider<Api> {
	const models = native.getModels().map((model) =>
		model.id === "deepseek-v4.1-flash" && model.name === "DeepSeek V4.1 Flash"
			? { ...model, name: "DeepSeek V4.1 Flash 1017" }
			: model,
	);
	return {
		...native,
		getModels: () => models,
		getAllModels: () => models,
	};
}

function buildWrappedOpenCodePool(
	options: PoolBuildOptions,
	providerId: string,
	nativeId: string,
	displayName: string,
	defaultRootUrl: string,
	hooks: { onFailover?: PoolBuildOptions["onFailover"]; onAccountFailure?: PoolBuildOptions["onAccountFailure"] },
): Provider<Api> {
	const native = builtinProviders().find((provider) => provider.id === nativeId);
	if (!native) throw new Error(`This Pi version does not include the native ${nativeId} provider`);
	return poolProvider(
		normalizeOpenCodeBaseProvider(native as Provider<Api>),
		providerId,
		displayName,
		options.accounts,
		options.state,
		options.resolveSecret,
		{ ...hooks, resolveModel: resolveOpenCodeModel(defaultRootUrl) },
	);
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
	if (channelKind(options.channel) === "opencode-go") {
		return buildWrappedOpenCodePool(
			options,
			providerId,
			"opencode-go",
			"OpenCode Go accounts (Subswap)",
			OPENCODE_GO_BASE_URL,
			hooks,
		);
	}
	if (channelKind(options.channel) === "opencode-zen") {
		return buildWrappedOpenCodePool(
			options,
			providerId,
			"opencode",
			"OpenCode Zen accounts (Subswap)",
			OPENCODE_ZEN_BASE_URL,
			hooks,
		);
	}
	if (channelKind(options.channel) === "volcengine") {
		const endpoint = volcengineEndpoint(first.endpoint);
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
			models: volcengineChannelModels(providerId, endpoint, options.accounts),
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

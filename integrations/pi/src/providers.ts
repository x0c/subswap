import {
	createProvider,
	type Api,
	type Model,
	type Provider,
} from "@earendil-works/pi-ai";
import { openAICompletionsApi } from "@earendil-works/pi-ai/compat";
import { builtinProviders } from "@earendil-works/pi-ai/providers/all";
import { poolProvider, type FailoverNotice, type PoolState } from "./pool.ts";
import type { RequestSecret, SubswapAccount } from "./subswap.ts";

export const SUPPORTED_SUBSWAP_PROVIDERS = ["codex", "volcengine"] as const;

export type SupportedSubswapProvider = (typeof SUPPORTED_SUBSWAP_PROVIDERS)[number];

export interface PoolRuntime {
	subswapProvider: SupportedSubswapProvider;
	providerId: string;
	accounts: SubswapAccount[];
	state: PoolState;
}

export interface PoolBuildOptions {
	accounts: SubswapAccount[];
	state: PoolState;
	resolveSecret: (account: SubswapAccount, signal: AbortSignal) => Promise<RequestSecret>;
	onFailover?: (notice: FailoverNotice) => void;
	onAccountFailure?: (account: SubswapAccount) => void;
}

const VOLCENGINE_BASE_URL = "https://ark.cn-beijing.volces.com/api/coding/v3";

function volcengineModel(): Model<"openai-completions"> {
	return {
		id: "kimi-k3.1-pro",
		name: "Kimi K3.1 Pro",
		api: "openai-completions",
		provider: "subswap-volcengine",
		baseUrl: VOLCENGINE_BASE_URL,
		reasoning: true,
		thinkingLevelMap: { xhigh: "xhigh", max: "max" },
		input: ["text", "image"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 1_024_000,
		maxTokens: 1_024_000,
	};
}

export function isSupportedProvider(provider: string): provider is SupportedSubswapProvider {
	return (SUPPORTED_SUBSWAP_PROVIDERS as readonly string[]).includes(provider);
}

export function buildPool(options: PoolBuildOptions & { subswapProvider: SupportedSubswapProvider }): Provider<Api> {
	const first = options.accounts[0];
	if (!first) throw new Error(`Subswap ${options.subswapProvider} pool is empty`);
	if (options.subswapProvider === "codex") {
		const native = builtinProviders().find((provider) => provider.id === "openai-codex");
		if (!native) throw new Error("This Pi version does not include the native Codex provider");
		return poolProvider(
			native as Provider<Api>,
			"subswap-codex",
			"Codex accounts (Subswap)",
			options.accounts,
			options.state,
			options.resolveSecret,
			{ onFailover: options.onFailover, onAccountFailure: options.onAccountFailure },
		);
	}

	const base = createProvider({
		id: "subswap-volcengine",
		name: "Volcengine Coding Plan (Subswap)",
		baseUrl: VOLCENGINE_BASE_URL,
		auth: {
			apiKey: {
				name: "Volcengine key managed by Subswap",
				async resolve() { return undefined; },
			},
		},
		models: [volcengineModel()],
		api: openAICompletionsApi(),
	});
	return poolProvider(
		base as Provider<Api>,
		"subswap-volcengine",
		"Volcengine Coding Plan (Subswap)",
		options.accounts,
		options.state,
		options.resolveSecret,
		{ onFailover: options.onFailover, onAccountFailure: options.onAccountFailure },
	);
}

export function buildPools(
	accounts: SubswapAccount[],
	states: Map<SupportedSubswapProvider, PoolState>,
	resolveSecret: PoolBuildOptions["resolveSecret"],
	onFailover: PoolBuildOptions["onFailover"] = undefined,
	onAccountFailure: PoolBuildOptions["onAccountFailure"] = undefined,
): Array<{ provider: Provider<Api>; runtime: PoolRuntime }> {
	const output: Array<{ provider: Provider<Api>; runtime: PoolRuntime }> = [];
	for (const subswapProvider of SUPPORTED_SUBSWAP_PROVIDERS) {
		const poolAccounts = accounts.filter((account) => account.provider === subswapProvider);
		if (poolAccounts.length === 0) continue;
		const state = states.get(subswapProvider);
		if (!state) throw new Error(`Missing state for Subswap ${subswapProvider}`);
		const providerId = `subswap-${subswapProvider}`;
		output.push({
			provider: buildPool({ subswapProvider, accounts: poolAccounts, state, resolveSecret, onFailover, onAccountFailure }),
			runtime: { subswapProvider, providerId, accounts: poolAccounts, state },
		});
	}
	return output;
}

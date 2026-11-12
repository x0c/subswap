import assert from "node:assert/strict";
import { test } from "node:test";
import {
	createAssistantMessageEventStream,
	createProvider,
	normalizeContext,
} from "@earendil-works/pi-ai";
import { newPoolState, orderedAccounts, poolProvider } from "../src/pool.ts";
import {
	buildPool,
	buildPools,
	channelKind,
	providerIdForChannel,
} from "../src/providers.ts";
import {
	channelOf,
	DEFAULT_CODEX_CHANNEL,
	DEFAULT_VOLCENGINE_CHANNEL,
	fallbackChannel,
	parseAuth,
	parseStatus,
	VOLCENGINE_BASE_URL,
	VOLCENGINE_MODEL,
	type SubswapAccount,
} from "../src/subswap.ts";

function account(overrides: Partial<SubswapAccount> & { id: string }): SubswapAccount {
	return {
		provider: "codex",
		displayLabel: overrides.id,
		active: false,
		manualOnly: false,
		...overrides,
	};
}

test("parseStatus prefers the server channel over the local fallback", () => {
	const [parsed] = parseStatus({
		accounts: [{
			id: "custom-1",
			provider: "codex",
			display_label: "Custom",
			active: true,
			manual_only: false,
			channel: "codex:custom-1",
			endpoint: "https://custom.example/v1",
			model: "custom-model",
			wire_api: "responses",
		}],
	});
	assert.ok(parsed);
	assert.equal(parsed.channel, "codex:custom-1");
	assert.equal(parsed.endpoint, "https://custom.example/v1");
	assert.equal(parsed.model, "custom-model");
	assert.equal(parsed.wireApi, "responses");
	assert.equal(channelOf(parsed), "codex:custom-1");
});

test("old-JSON fallback groups codex OAuth, codex custom, volcengine, and unknown", () => {
	assert.equal(
		fallbackChannel({ provider: "codex", id: "oauth", manualOnly: false }),
		DEFAULT_CODEX_CHANNEL,
	);
	assert.equal(
		fallbackChannel({ provider: "codex", id: "custom-1", manualOnly: true }),
		"codex:custom-1",
	);
	assert.equal(
		fallbackChannel({
			provider: "codex",
			id: "custom-2",
			endpoint: "https://custom.example/v1",
			model: "m",
		}),
		"codex:custom-2",
	);
	assert.equal(
		fallbackChannel({ provider: "volcengine", id: "ark" }),
		DEFAULT_VOLCENGINE_CHANNEL,
	);
	assert.equal(
		fallbackChannel({
			provider: "volcengine",
			id: "ark",
			endpoint: "https://ark.example/v3",
			model: "other-model",
		}),
		"volcengine:https://ark.example/v3#other-model",
	);
	assert.equal(fallbackChannel({ provider: "claude", id: "c1" }), "claude");
	assert.equal(
		fallbackChannel({ provider: "claude", id: "c1", endpoint: "https://x", model: "m" }),
		"claude:https://x#m",
	);
});

test("parseAuth exposes endpoint and model from credential and environment shapes", () => {
	assert.deepEqual(
		parseAuth({
			kind: "credential",
			secret: "key",
			endpoint: "https://custom.example/v1",
			model: "custom-model",
		}),
		{ apiKey: "key", endpoint: "https://custom.example/v1", model: "custom-model" },
	);
	assert.deepEqual(
		parseAuth({
			kind: "environment",
			env: {
				ANTHROPIC_AUTH_TOKEN: "ark-key",
				ANTHROPIC_BASE_URL: "https://ark.example/api/coding",
				ANTHROPIC_MODEL: "env-model",
			},
		}),
		{ apiKey: "ark-key", endpoint: "https://ark.example/api/coding", model: "env-model" },
	);
});

test("orderedAccounts filters by channel and gives cross-channel zero participation", () => {
	const accounts = [
		account({ id: "a", channel: "codex" }),
		account({ id: "b", channel: "codex:custom-1" }),
	];
	const state = newPoolState();
	assert.deepEqual(
		orderedAccounts(accounts, state, new Set(), Date.now(), "codex").map((entry) => entry.id),
		["a"],
	);
	assert.deepEqual(
		orderedAccounts(accounts, state, new Set(), Date.now(), "codex:custom-1").map((entry) => entry.id),
		["b"],
	);
	assert.deepEqual(
		orderedAccounts(accounts, state, new Set(), Date.now(), "volcengine:missing").map((entry) => entry.id),
		[],
	);
});

test("a mixed-channel pool refuses to stream before resolving any secret", async () => {
	let secretCalls = 0;
	const base = createProvider({
		id: "mixed-base",
		auth: {
			apiKey: {
				name: "outer",
				async resolve() { return { auth: { apiKey: "outer" }, source: "test" }; },
			},
		},
		models: [{
			id: "m",
			name: "M",
			api: "test-api",
			provider: "mixed-base",
			baseUrl: "https://test.invalid",
			reasoning: false,
			input: ["text"],
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
			contextWindow: 10,
			maxTokens: 10,
		}],
		api: {
			stream: () => {
				const stream = createAssistantMessageEventStream();
				queueMicrotask(() => stream.end({} as never));
				return stream;
			},
			streamSimple: () => {
				const stream = createAssistantMessageEventStream();
				queueMicrotask(() => stream.end({} as never));
				return stream;
			},
		},
	});
	const pooled = poolProvider(
		base,
		"subswap-mixed",
		"Subswap mixed",
		[account({ id: "a", channel: "codex" }), account({ id: "b", channel: "codex:custom-1" })],
		newPoolState(),
		async () => {
			secretCalls++;
			return { apiKey: "unused" };
		},
	);
	const context = normalizeContext({ messages: [{ role: "user", content: "hi", timestamp: 1 }] });
	const model = pooled.getModels()[0];
	assert.ok(model);
	const stream = pooled.stream(model, context, {});
	const events = [];
	for await (const event of stream) events.push(event);
	const result = await stream.result();
	assert.equal(secretCalls, 0);
	assert.match(result.errorMessage ?? "", /mixes channels/);
});

test("endpoint decides the Pi baseUrl for volcengine and codex custom channels", () => {
	const volcengine = buildPool({
		channel: "volcengine:https://ark.example/api/coding#other-model",
		accounts: [account({
			id: "ark",
			provider: "volcengine",
			channel: "volcengine:https://ark.example/api/coding#other-model",
			endpoint: "https://ark.example/api/coding",
			model: "other-model",
		})],
		state: newPoolState(),
		resolveSecret: async () => ({ apiKey: "key" }),
	});
	const volcengineModel = volcengine.getModels()[0];
	assert.ok(volcengineModel);
	// The Anthropic-compatible base normalizes to the OpenAI-compatible /v3 path.
	assert.equal(volcengineModel.baseUrl, "https://ark.example/api/coding/v3");
	assert.equal(volcengineModel.id, "other-model");

	const customChannel = "codex:custom-1";
	const custom = buildPool({
		channel: customChannel,
		accounts: [account({
			id: "custom-1",
			channel: customChannel,
			endpoint: "https://custom.example/v1",
			model: "custom-model",
		})],
		state: newPoolState(),
		resolveSecret: async () => ({ apiKey: "key" }),
	});
	const customModel = custom.getModels()[0];
	assert.ok(customModel);
	assert.equal(customModel.baseUrl, "https://custom.example/v1");
	assert.equal(customModel.id, "custom-model");
	assert.equal(customModel.api, "openai-responses");
});

test("codex custom without endpoint metadata stays discovered but unlisted", () => {
	assert.throws(
		() => buildPool({
			channel: "codex:custom-1",
			accounts: [account({ id: "custom-1", channel: "codex:custom-1" })],
			state: newPoolState(),
			resolveSecret: async () => ({ apiKey: "key" }),
		}),
		/has no endpoint metadata/,
	);
});

test("pools group by channel, keep default ids, and skip unsupported channels", () => {
	const accounts: SubswapAccount[] = [
		account({ id: "oauth", channel: "codex" }),
		account({ id: "custom-1", channel: "codex:custom-1", endpoint: "https://custom.example/v1", model: "m1" }),
		account({
			id: "ark",
			provider: "volcengine",
			displayLabel: "ark",
			channel: DEFAULT_VOLCENGINE_CHANNEL,
		}),
		account({ id: "c1", provider: "claude", displayLabel: "c1", channel: "claude" }),
		account({ id: "bare-custom", channel: "codex:bare-custom" }),
	];
	const states = new Map([
		["codex", newPoolState()],
		["codex:custom-1", newPoolState()],
		[DEFAULT_VOLCENGINE_CHANNEL, newPoolState()],
		["claude", newPoolState()],
		["codex:bare-custom", newPoolState()],
	]);
	const pools = buildPools(accounts, states, async () => ({ apiKey: "key" }));
	const byChannel = new Map(pools.map((entry) => [entry.runtime.channel, entry.provider.id]));
	assert.equal(byChannel.get("codex"), "subswap-codex");
	assert.equal(byChannel.get(DEFAULT_VOLCENGINE_CHANNEL), "subswap-volcengine");
	assert.equal(byChannel.get("codex:custom-1"), providerIdForChannel("codex:custom-1"));
	assert.equal(byChannel.has("claude"), false);
	assert.equal(byChannel.has("codex:bare-custom"), false);
	assert.equal(channelKind("claude"), "unsupported");
	assert.equal(channelKind(VOLCENGINE_BASE_URL), "unsupported");
	assert.equal(providerIdForChannel(DEFAULT_VOLCENGINE_CHANNEL), "subswap-volcengine");
	assert.equal(VOLCENGINE_MODEL, "kimi-k3.1-pro");
});

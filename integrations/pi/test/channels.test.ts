import assert from "node:assert/strict";
import { createServer, type IncomingHttpHeaders } from "node:http";
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
	DEFAULT_OPENCODE_GO_CHANNEL,
	DEFAULT_OPENCODE_ZEN_CHANNEL,
	DEFAULT_VOLCENGINE_CHANNEL,
	fallbackChannel,
	OPENCODE_GO_BASE_URL,
	OPENCODE_ZEN_BASE_URL,
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

test("parseStatus keeps the quota windows that decide which account can serve", () => {
	const [exhausted, healthy, legacy] = parseStatus({
		accounts: [
			{
				id: "goru",
				provider: "pi",
				active: false,
				channel: DEFAULT_OPENCODE_GO_CHANNEL,
				quotas: [
					{ provider: "pi", window: "month", used: 100, limit: 100, reset_at: "2026-11-25T11:46:03Z", status: "exhausted" },
					{ provider: "pi", window: "five_hour", used: 0, limit: 100, reset_at: "2026-11-22T18:30:31.210Z", status: "ok" },
				],
			},
			{
				id: "1hvn",
				provider: "pi",
				active: false,
				channel: DEFAULT_OPENCODE_GO_CHANNEL,
				quotas: [{ window: "month", used: 4, limit: 100, reset_at: "2026-12-22T06:47:23Z", status: "ok" }],
			},
			{ id: "ark", provider: "volcengine", active: true },
		],
	});
	assert.ok(exhausted && healthy && legacy);
	assert.deepEqual(exhausted.quotas?.map((quota) => quota.window), ["month", "five_hour"]);
	assert.equal(exhausted.quotas?.[0]?.status, "exhausted");
	assert.equal(exhausted.quotas?.[0]?.resetAtMs, Date.parse("2026-11-25T11:46:03Z"));
	// Millisecond-precision reset instants survive, and unreadable entries are dropped, not guessed.
	assert.equal(exhausted.quotas?.[1]?.resetAtMs, Date.parse("2026-11-22T18:30:31.210Z"));
	assert.equal(healthy.quotas?.[0]?.used, 4);
	assert.equal(legacy.quotas, undefined);
	assert.deepEqual(parseStatus({ accounts: [{ id: "a", provider: "pi", quotas: [{ used: 3 }, "junk"] }] })[0]?.quotas, undefined);
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

test("opencode-go and opencode-zen build native catalog pools while console oauth stays unsupported", () => {
	assert.equal(channelKind(DEFAULT_OPENCODE_GO_CHANNEL), "opencode-go");
	assert.equal(channelKind("opencode-go:team-a"), "opencode-go");
	assert.equal(channelKind(DEFAULT_OPENCODE_ZEN_CHANNEL), "opencode-zen");
	assert.equal(channelKind("opencode-zen:team-b"), "opencode-zen");
	assert.equal(channelKind("opencode"), "unsupported");
	assert.equal(providerIdForChannel(DEFAULT_OPENCODE_GO_CHANNEL), "subswap-opencode-go");
	assert.equal(providerIdForChannel(DEFAULT_OPENCODE_ZEN_CHANNEL), "subswap-opencode-zen");

	const consoleOauth = account({ id: "console-oauth", provider: "opencode", displayLabel: "oauth" });
	assert.equal(channelOf(consoleOauth), "opencode");

	const accounts: SubswapAccount[] = [
		consoleOauth,
		account({
			id: "go-1",
			provider: "pi",
			displayLabel: "opencode-go …1111",
			channel: DEFAULT_OPENCODE_GO_CHANNEL,
			endpoint: OPENCODE_GO_BASE_URL,
			model: "kimi-k3.1-pro",
		}),
		account({
			id: "zen-1",
			provider: "pi",
			displayLabel: "opencode-zen …2222",
			channel: DEFAULT_OPENCODE_ZEN_CHANNEL,
			endpoint: OPENCODE_ZEN_BASE_URL,
			model: "kimi-k3.1-pro",
		}),
	];
	const states = new Map([
		["opencode", newPoolState()],
		[DEFAULT_OPENCODE_GO_CHANNEL, newPoolState()],
		[DEFAULT_OPENCODE_ZEN_CHANNEL, newPoolState()],
	]);
	const pools = buildPools(accounts, states, async () => ({ apiKey: "oc_sk_test" }));
	const byProviderId = new Map(pools.map((entry) => [entry.provider.id, entry.provider]));
	assert.equal(byProviderId.has("subswap-opencode"), false);

	const goProvider = byProviderId.get("subswap-opencode-go");
	assert.ok(goProvider, "expected subswap-opencode-go provider");
	const goModelIds = new Set(goProvider.getModels().map((m) => m.id));
	for (const required of ["kimi-k3.1-pro", "deepseek-v4.1-flash", "deepseek-v4-pro", "glm-5.3", "gemini-4-argon", "gpt-6-luna"]) {
		assert.ok(goModelIds.has(required), `missing opencode-go model ${required}`);
	}
	const deepseekFlash = goProvider.getModels().find((m) => m.id === "deepseek-v4.1-flash");
	assert.equal(deepseekFlash?.name, "DeepSeek V4.1 Flash 1017 (Subswap)");

	const zenProvider = byProviderId.get("subswap-opencode-zen");
	assert.ok(zenProvider, "expected subswap-opencode-zen provider");
	assert.ok(zenProvider.getModels().length > 0);
});

test("a rejected attempt records its transport status and Retry-After cooldown", async () => {
	const server = createServer((req, res) => {
		const chunks: Buffer[] = [];
		req.on("data", (chunk: Buffer) => chunks.push(chunk));
		req.on("end", () => {
			if (req.headers.authorization === "Bearer oc_sk_spent") {
				// The real OpenCode Go monthly refusal: 429 JSON plus a multi-day Retry-After.
				res.writeHead(429, { "content-type": "application/json", "retry-after": "7200" });
				res.end(JSON.stringify({
					type: "error",
					error: { type: "GoUsageLimitError", message: "OpenCode Go usage limit reached (monthly, resets in 2d 22h)." },
				}));
				return;
			}
			res.writeHead(200, { "content-type": "text/event-stream; charset=utf-8" });
			res.write('data: {"id":"c1","object":"chat.completion.chunk","created":1,"choices":[{"index":0,"delta":{"role":"assistant","content":"spent-then-healthy"},"finish_reason":null}]}\n\n');
			res.write('data: {"id":"c1","object":"chat.completion.chunk","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":3,"total_tokens":7}}\n\n');
			res.end("data: [DONE]\n\n");
		});
	});
	await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
	const address = server.address();
	if (!address || typeof address === "string") throw new Error("unexpected server address");
	const mockRoot = `http://127.0.0.1:${address.port}`;

	try {
		const state = newPoolState();
		const failovers: Array<{ reason: string }> = [];
		const pool = buildPool({
			channel: DEFAULT_OPENCODE_GO_CHANNEL,
			accounts: [
				account({
					id: "go-spent",
					provider: "pi",
					channel: DEFAULT_OPENCODE_GO_CHANNEL,
					endpoint: mockRoot,
					model: "kimi-k3.1-pro",
				}),
				account({
					id: "go-fresh",
					provider: "pi",
					channel: DEFAULT_OPENCODE_GO_CHANNEL,
					endpoint: mockRoot,
					model: "kimi-k3.1-pro",
				}),
			],
			state,
			resolveSecret: async (acct) => ({ apiKey: acct.id === "go-spent" ? "oc_sk_spent" : "oc_sk_fresh" }),
			onFailover: (notice) => failovers.push({ reason: notice.reason }),
		});
		const kimiModel = pool.getModels().find((m) => m.id === "kimi-k3.1-pro");
		assert.ok(kimiModel);
		const context = normalizeContext({
			messages: [{ role: "user", content: "Reply with spent-then-healthy", timestamp: Date.now() }],
		});
		const before = Date.now();
		const result = await pool.streamSimple(kimiModel, context, { sessionId: "sess-retry-after" }).result();
		assert.equal(result.stopReason, "stop", `unexpected error: ${result.errorMessage}`);
		// The failed response never reaches onResponse, so the pool must still see HTTP 429.
		assert.deepEqual(failovers.map((notice) => notice.reason), ["HTTP 429"]);
		const until = state.cooldowns.get("go-spent") ?? 0;
		const cooledMs = until - before;
		assert.ok(cooledMs >= 7_000_000, `cooldown ignored Retry-After: ${cooledMs}ms`);
		assert.ok(cooledMs <= 7_300_000, `cooldown overshot Retry-After: ${cooledMs}ms`);
	} finally {
		await new Promise<void>((resolve, reject) => server.close((err) => (err ? reject(err) : resolve())));
	}
});

test("subswap-opencode-go fails over across accounts over HTTP with bearer auth and session header", async () => {
	const seen: Array<{ url: string | undefined; headers: IncomingHttpHeaders }> = [];
	const server = createServer((req, res) => {
		const chunks: Buffer[] = [];
		req.on("data", (chunk: Buffer) => chunks.push(chunk));
		req.on("end", () => {
			seen.push({ url: req.url, headers: req.headers });
			const authHeader = req.headers.authorization;
			if (authHeader === "Bearer oc_sk_test_first") {
				res.writeHead(429, { "content-type": "application/json" });
				res.end(JSON.stringify({
					error: {
						message: "Go usage limit exceeded",
						type: "rate_limit_error",
						code: "GoUsageLimitError",
					},
				}));
				return;
			}
			res.writeHead(200, { "content-type": "text/event-stream; charset=utf-8" });
			res.write(
				'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"kimi-k3.1-pro","choices":[{"index":0,"delta":{"role":"assistant","content":"recovered-from-go-pool"},"finish_reason":null}]}\n\n',
			);
			res.write(
				'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"kimi-k3.1-pro","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":3,"total_tokens":7}}\n\n',
			);
			res.end("data: [DONE]\n\n");
		});
	});
	await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
	const address = server.address();
	if (!address || typeof address === "string") throw new Error("unexpected server address");
	const mockRoot = `http://127.0.0.1:${address.port}`;

	try {
		const accounts: SubswapAccount[] = [
			account({
				id: "go-1",
				provider: "pi",
				displayLabel: "opencode-go …0001",
				active: true,
				channel: DEFAULT_OPENCODE_GO_CHANNEL,
				endpoint: mockRoot,
				model: "kimi-k3.1-pro",
			}),
			account({
				id: "go-2",
				provider: "pi",
				displayLabel: "opencode-go …0002",
				active: false,
				channel: DEFAULT_OPENCODE_GO_CHANNEL,
				endpoint: mockRoot,
				model: "kimi-k3.1-pro",
			}),
		];
		const failovers: Array<{ from: string; to: string }> = [];
		const failed: string[] = [];
		const pool = buildPool({
			channel: DEFAULT_OPENCODE_GO_CHANNEL,
			accounts,
			state: newPoolState(),
			resolveSecret: async (acct) => ({
				apiKey: acct.id === "go-1" ? "oc_sk_test_first" : "oc_sk_test_second",
			}),
			onFailover: (notice) => failovers.push({ from: notice.from, to: notice.to }),
			onAccountFailure: (acct) => failed.push(acct.id),
		});
		const kimiModel = pool.getModels().find((m) => m.id === "kimi-k3.1-pro");
		assert.ok(kimiModel, "expected kimi-k3.1-pro in subswap-opencode-go");

		const context = normalizeContext({
			messages: [{ role: "user", content: "Reply with recovered-from-go-pool", timestamp: Date.now() }],
		});
		const stream = pool.streamSimple(kimiModel, context, { sessionId: "sess-oc-1" });
		const result = await stream.result();
		assert.equal(result.stopReason, "stop", `unexpected error: ${result.errorMessage}`);
		const text = result.content
			.filter((part): part is { type: "text"; text: string } => part.type === "text")
			.map((part) => part.text)
			.join("");
		assert.equal(text, "recovered-from-go-pool");
		assert.deepEqual(failed, ["go-1"]);
		assert.deepEqual(failovers, [{ from: "opencode-go …0001", to: "opencode-go …0002" }]);
		assert.equal(seen.length, 2);
		assert.equal(seen[0]?.url, "/openai/v1/chat/completions");
		assert.equal(seen[0]?.headers.authorization, "Bearer oc_sk_test_first");
		assert.equal(seen[0]?.headers["x-opencode-session"], "sess-oc-1");
		assert.equal(seen[1]?.url, "/openai/v1/chat/completions");
		assert.equal(seen[1]?.headers.authorization, "Bearer oc_sk_test_second");
		assert.equal(seen[1]?.headers["x-opencode-session"], "sess-oc-1");
	} finally {
		await new Promise<void>((resolve, reject) => server.close((err) => (err ? reject(err) : resolve())));
	}
});

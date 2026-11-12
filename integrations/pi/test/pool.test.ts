import assert from "node:assert/strict";
import { test } from "node:test";
import {
	createAssistantMessageEventStream,
	createModels,
	createProvider,
	type AssistantMessage,
	type AssistantMessageEvent,
	type AssistantMessageEventStream,
	type Model,
	normalizeContext,
	type ProviderResponse,
	type SimpleStreamOptions,
	type StopReason,
	type TranscriptContext,
} from "@earendil-works/pi-ai";
import { classifyAccountFailure, cooldownFor, newPoolState, poolProvider } from "../src/pool.ts";
import type { SubswapAccount } from "../src/subswap.ts";

const model: Model<"test-api"> = {
	id: "same-model",
	name: "Same Model",
	api: "test-api",
	provider: "test-provider",
	baseUrl: "https://test.invalid",
	reasoning: true,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 1000,
	maxTokens: 100,
};

const accounts: SubswapAccount[] = [
	{ id: "a", provider: "codex", displayLabel: "Account A", active: true, manualOnly: false, channel: "codex" },
	{ id: "b", provider: "codex", displayLabel: "Account B", active: false, manualOnly: false, channel: "codex" },
	{ id: "c", provider: "codex", displayLabel: "Account C", active: false, manualOnly: true, channel: "codex" },
];

function message(stopReason: StopReason, text = ""): AssistantMessage {
	return {
		role: "assistant",
		content: text ? [{ type: "text", text }] : [],
		api: model.api,
		provider: model.provider,
		model: model.id,
		usage: {
			input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason,
		timestamp: Date.now(),
	};
}

function error(stream: AssistantMessageEventStream, errorMessage: string): void {
	const failed = { ...message("error"), errorMessage };
	stream.push({ type: "error", reason: "error", error: failed });
	stream.end(failed);
}

function success(stream: AssistantMessageEventStream, text: string): void {
	const partial = message("pending", text);
	stream.push({ type: "start", partial });
	stream.push({ type: "text_start", contentIndex: 0, partial });
	stream.push({ type: "text_delta", contentIndex: 0, delta: text, partial });
	stream.push({ type: "text_end", contentIndex: 0, content: text, partial });
	const done = message("stop", text);
	stream.push({ type: "done", reason: "stop", message: done });
	stream.end(done);
}

type Handler = (
	requestModel: Model<"test-api">,
	context: TranscriptContext,
	options?: SimpleStreamOptions,
) => AssistantMessageEventStream;

function fixture(handler: Handler, poolAccounts = accounts) {
	const attempts: Array<{
		key: string;
		provider: string;
		model: string;
		context: TranscriptContext;
		reasoning: SimpleStreamOptions["reasoning"];
		maxRetries: number | undefined;
		header: string | null | undefined;
	}> = [];
	const base = createProvider({
		id: "test-provider",
		auth: {
			apiKey: {
				name: "outer",
				async resolve() { return { auth: { apiKey: "outer-placeholder" }, source: "test" }; },
			},
		},
		models: [model],
		api: {
			stream: handler,
			streamSimple: handler,
		},
	});
	const state = newPoolState();
	const provider = poolProvider(
		base,
		"subswap-test",
		"Subswap test",
		poolAccounts,
		state,
		async (account) => ({ apiKey: `secret-${account.id}` }),
	);
	const models = createModels();
	models.setProvider(provider);
	const selected = models.getModel("subswap-test", model.id);
	assert.ok(selected);
	const context = normalizeContext({
		messages: [{ role: "user", content: "Original task", timestamp: 1 }],
	});
	return {
		state,
		context,
		async complete(options?: SimpleStreamOptions) {
			const wrapped: Handler = (requestModel, requestContext, requestOptions) => {
				attempts.push({
					key: requestOptions?.apiKey ?? "",
					provider: requestModel.provider,
					model: requestModel.id,
					context: requestContext,
					reasoning: requestOptions?.reasoning,
					maxRetries: requestOptions?.maxRetries,
					header: requestOptions?.headers?.["x-test"],
				});
				return handler(requestModel, requestContext, requestOptions);
			};
			const replacement = createProvider({
				id: "test-provider",
				auth: base.auth,
				models: [model],
				api: { stream: wrapped, streamSimple: wrapped },
			});
			const pooled = poolProvider(
				replacement,
				"subswap-test",
				"Subswap test",
				poolAccounts,
				state,
				async (account) => ({ apiKey: `secret-${account.id}` }),
			);
			const registry = createModels();
			registry.setProvider(pooled);
			const selectedModel = registry.getModel("subswap-test", model.id);
			assert.ok(selectedModel);
			return registry.completeSimple(selectedModel, context, {
				reasoning: "xhigh",
				headers: { "x-test": "preserved" },
				...options,
			});
		},
		attempts,
	};
}

function startAndError(stream: AssistantMessageEventStream, errorMessage: string, status = 429, responseModel?: Model<"test-api">, options?: SimpleStreamOptions): void {
	stream.push({ type: "start", partial: message("pending") });
	if (responseModel && options) {
		const response: ProviderResponse = { status, headers: { "retry-after": "60" } };
		void Promise.resolve(options.onResponse?.(response, responseModel)).then(() => error(stream, errorMessage));
	} else {
		error(stream, errorMessage);
	}
}

test("quota refusal rotates inside one request without changing task, model, reasoning, or headers", async () => {
	const run = fixture((_requestModel, _context, options) => {
		const stream = createAssistantMessageEventStream();
		queueMicrotask(() => {
			if (options?.apiKey === "secret-a") {
				startAndError(stream, "usage_limit_reached", 429, model, options);
			} else {
				success(stream, "continued");
			}
		});
		return stream;
	});
	const result = await run.complete();
	assert.equal(result.stopReason, "stop");
	assert.deepEqual(result.content, [{ type: "text", text: "continued" }]);
	assert.deepEqual(run.attempts.map((attempt) => attempt.key), ["secret-a", "secret-b"]);
	assert.equal(run.state.lastHealthyId, "b");
	for (const attempt of run.attempts) {
		assert.equal(attempt.provider, "subswap-test");
		assert.equal(attempt.model, "same-model");
		assert.deepEqual(attempt.context, run.context);
		assert.equal(attempt.reasoning, "xhigh");
		assert.equal(attempt.maxRetries, 0);
		assert.equal(attempt.header, "preserved");
	}
});

for (const status of [undefined, 200]) {
	test(`normalized Codex stream quota refusal rotates with transport status ${status ?? "absent"}`, async () => {
		const run = fixture((requestModel, _context, options) => {
			const stream = createAssistantMessageEventStream();
			queueMicrotask(() => {
				if (options?.apiKey === "secret-a") {
					if (status === undefined) {
						startAndError(stream, "Codex error: The usage limit has been reached");
					} else {
						startAndError(stream, "Codex error: The usage limit has been reached", status, requestModel, options);
					}
				} else {
					success(stream, "continued");
				}
			});
			return stream;
		}, accounts.slice(0, 2));
		const result = await run.complete();
		assert.equal(result.stopReason, "stop");
		assert.deepEqual(result.content, [{ type: "text", text: "continued" }]);
		assert.deepEqual(run.attempts.map((attempt) => attempt.key), ["secret-a", "secret-b"]);
		assert.equal(run.state.lastHealthyId, "b");
		assert.ok(run.state.cooldowns.has("a"));
		assert.ok(run.attempts.every((attempt) => attempt.model === "same-model" && attempt.reasoning === "xhigh"));
		for (const attempt of run.attempts) assert.deepEqual(attempt.context, run.context);
	});
}

test("a transient non-account failure stays on the current account", async () => {
	const run = fixture((_requestModel, _context, options) => {
		const stream = createAssistantMessageEventStream();
		queueMicrotask(() => startAndError(stream, "HTTP 503 upstream", 503));
		return stream;
	});
	const result = await run.complete();
	assert.equal(result.stopReason, "error");
	assert.deepEqual(run.attempts.map((attempt) => attempt.key), ["secret-a"]);
});

test("no account is switched after visible output", async () => {
	const run = fixture((_requestModel, _context, options) => {
		const stream = createAssistantMessageEventStream();
		queueMicrotask(() => {
			const partial = message("pending", "partial");
			stream.push({ type: "start", partial });
			stream.push({ type: "text_start", contentIndex: 0, partial });
			stream.push({ type: "text_delta", contentIndex: 0, delta: "partial", partial });
			const failed = { ...partial, stopReason: "error" as const, errorMessage: "429 rate limit after output" };
			stream.push({ type: "error", reason: "error", error: failed });
			stream.end(failed);
		});
		return stream;
	});
	const result = await run.complete();
	assert.equal(result.stopReason, "error");
	assert.deepEqual(run.attempts.map((attempt) => attempt.key), ["secret-a"]);
	assert.deepEqual(result.content, [{ type: "text", text: "partial" }]);
});

test("all accounts are tried once and never oscillate A-B-A", async () => {
	const run = fixture((_requestModel, _context, options) => {
		const stream = createAssistantMessageEventStream();
		queueMicrotask(() => startAndError(stream, "quota exhausted", 429, model, options));
		return stream;
	});
	const result = await run.complete();
	assert.equal(result.stopReason, "error");
	assert.deepEqual(run.attempts.map((attempt) => attempt.key), ["secret-a", "secret-b", "secret-c"]);
	assert.equal(run.state.cooldowns.size, 3);
});

test("manual-only API keys participate in request-scoped failover", async () => {
	const pool = accounts.slice(1);
	const run = fixture((_requestModel, _context, options) => {
		const stream = createAssistantMessageEventStream();
		queueMicrotask(() => {
			if (options?.apiKey === "secret-b") startAndError(stream, "AccountQuotaExceeded", 429, model, options);
			else success(stream, "manual key continued");
		});
		return stream;
	}, pool);
	const result = await run.complete();
	assert.equal(result.stopReason, "stop");
	assert.deepEqual(run.attempts.map((attempt) => attempt.key), ["secret-b", "secret-c"]);
});

test("abort is not classified as a failed account", async () => {
	const run = fixture(() => {
		const stream = createAssistantMessageEventStream();
		queueMicrotask(() => {
			const aborted = { ...message("aborted"), errorMessage: "aborted" };
			stream.push({ type: "start", partial: message("pending") });
			stream.push({ type: "error", reason: "aborted", error: aborted });
			stream.end(aborted);
		});
		return stream;
	});
	const result = await run.complete();
	assert.equal(result.stopReason, "aborted");
	assert.deepEqual(run.attempts.map((attempt) => attempt.key), ["secret-a"]);
	assert.equal(run.state.cooldowns.size, 0);
});

test("error classification and retry-after cooldown are conservative", () => {
	assert.equal(classifyAccountFailure("AccountQuotaExceeded", 429), true);
	assert.equal(classifyAccountFailure("invalid api key", 401), true);
	assert.equal(classifyAccountFailure("context length exceeded", 400), false);
	assert.equal(classifyAccountFailure("fetch failed"), false);
	assert.equal(cooldownFor("quota", 429, { "retry-after": "120" }, 1000), 301_000);
	assert.equal(cooldownFor("quota", 429, { "retry-after": "600" }, 1000), 601_000);
	assert.equal(cooldownFor("auth", 401, undefined, 1000), 61_000);
});

test("stream quota classification accepts successful transports and preserves unrelated errors", () => {
	for (const status of [undefined, 200, 201]) {
		assert.equal(classifyAccountFailure("Codex error: The usage limit has been reached", status), true);
		assert.equal(classifyAccountFailure("usage_limit_reached", status), true);
		assert.equal(classifyAccountFailure("Usage limit reached", status), true);
		assert.equal(classifyAccountFailure("context length exceeded", status), false);
		assert.equal(classifyAccountFailure("fetch failed", status), false);
		assert.equal(classifyAccountFailure("usage limit configuration invalid", status), false);
	}
	for (const status of [400, 404, 500, 503]) {
		assert.equal(classifyAccountFailure("Codex error: The usage limit has been reached", status), false);
	}
});

test("retry-after lookup is ASCII case-insensitive", () => {
	// 600 秒超过 5 分钟基准，可区分“找到并采用”与“回退基准”。
	assert.equal(cooldownFor("quota", 429, { "Retry-After": "600" }, 1000), 601_000);
	const now = 1_700_000_000_000;
	const date = new Date(now + 600_000).toUTCString();
	assert.equal(cooldownFor("quota", 429, { "RETRY-AFTER": date }, now), now + 600_000);
	assert.equal(cooldownFor("quota", 429, { "Retry-After": "not-a-date" }, 1000), 301_000);
});

// 直接驱动 pooled stream：需要自定义 signal 与 setup 失败，不走 Models 层。
async function runDirect(
	handler: Handler,
	poolAccounts: SubswapAccount[],
	resolveSecret: (account: SubswapAccount, signal: AbortSignal) => Promise<{ apiKey: string }>,
	options?: SimpleStreamOptions,
	hooks?: { onAccountFailure?: (account: SubswapAccount) => void; onFailover?: () => void },
) {
	const base = createProvider({
		id: "test-provider",
		auth: {
			apiKey: {
				name: "outer",
				async resolve() { return { auth: { apiKey: "outer-placeholder" }, source: "test" }; },
			},
		},
		models: [model],
		api: { stream: handler, streamSimple: handler },
	});
	const state = newPoolState();
	const pooled = poolProvider(
		base,
		"subswap-test",
		"Subswap test",
		poolAccounts,
		state,
		resolveSecret,
		hooks ?? {},
	);
	const context = normalizeContext({
		messages: [{ role: "user", content: "Direct task", timestamp: 1 }],
	});
	const stream = pooled.stream(model, context, { ...(options ?? {}) } as SimpleStreamOptions & Record<string, unknown>);
	const events: AssistantMessageEvent[] = [];
	for await (const event of stream) events.push(event);
	return { state, events, result: await stream.result() };
}

test("pre-aborted signal ends aborted without touching any account", async () => {
	let secretCalls = 0;
	const controller = new AbortController();
	controller.abort();
	const run = await runDirect(
		() => {
			const stream = createAssistantMessageEventStream();
			queueMicrotask(() => error(stream, "unreachable"));
			return stream;
		},
		accounts,
		async () => {
			secretCalls++;
			return { apiKey: "unused" };
		},
		{ signal: controller.signal },
	);
	assert.equal(run.result.stopReason, "aborted");
	assert.ok(run.events.some((event) => event.type === "error" && event.reason === "aborted"));
	assert.equal(secretCalls, 0);
	assert.equal(run.state.cooldowns.size, 0);
});

test("abort during credential resolution surfaces aborted and rolls back cooldowns", async () => {
	const controller = new AbortController();
	let accountFailures = 0;
	const run = await runDirect(
		(_requestModel, _context, options) => {
			const stream = createAssistantMessageEventStream();
			queueMicrotask(() => startAndError(stream, "usage_limit_reached", 429, model, options));
			return stream;
		},
		accounts.slice(0, 2),
		async (account, signal) => {
			if (account.id === "b") {
				controller.abort();
				if (signal.aborted) throw new DOMException("This operation was aborted", "AbortError");
			}
			return { apiKey: `secret-${account.id}` };
		},
		{ signal: controller.signal },
		{ onAccountFailure: () => { accountFailures++; } },
	);
	assert.equal(run.result.stopReason, "aborted");
	assert.ok(run.events.some((event) => event.type === "error" && event.reason === "aborted"));
	assert.equal(run.state.cooldowns.size, 0);
	assert.equal(accountFailures, 1);
});

test("setup failure after an HTTP rejection surfaces the setup error", async () => {
	const run = await runDirect(
		(_requestModel, _context, options) => {
			const stream = createAssistantMessageEventStream();
			queueMicrotask(() => startAndError(stream, "usage_limit_reached", 429, model, options));
			return stream;
		},
		accounts.slice(0, 2),
		async (account) => {
			if (account.id === "b") throw new Error("Subswap auth failed");
			return { apiKey: `secret-${account.id}` };
		},
	);
	assert.equal(run.result.stopReason, "error");
	assert.equal(run.result.errorMessage, "Subswap auth failed");
});

test("HTTP rejection after a setup failure surfaces the provider event", async () => {
	const run = await runDirect(
		(_requestModel, _context, options) => {
			const stream = createAssistantMessageEventStream();
			queueMicrotask(() => startAndError(stream, "usage_limit_reached", 429, model, options));
			return stream;
		},
		accounts.slice(0, 2),
		async (account) => {
			if (account.id === "a") throw new Error("Subswap auth failed");
			return { apiKey: `secret-${account.id}` };
		},
	);
	assert.equal(run.result.stopReason, "error");
	assert.equal(run.result.errorMessage, "usage_limit_reached");
	assert.equal(run.events.filter((event) => event.type === "start").length, 1);
});

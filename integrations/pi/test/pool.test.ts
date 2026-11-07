import assert from "node:assert/strict";
import { test } from "node:test";
import {
	createAssistantMessageEventStream,
	createModels,
	createProvider,
	type AssistantMessage,
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
	{ id: "a", provider: "codex", displayLabel: "Account A", active: true, manualOnly: false },
	{ id: "b", provider: "codex", displayLabel: "Account B", active: false, manualOnly: false },
	{ id: "c", provider: "codex", displayLabel: "Account C", active: false, manualOnly: true },
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

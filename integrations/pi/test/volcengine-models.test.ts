import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";
import { normalizeContext } from "@earendil-works/pi-ai";
import { newPoolState } from "../src/pool.ts";
import { buildPool, providerIdForChannel } from "../src/providers.ts";
import {
	DEFAULT_VOLCENGINE_CHANNEL,
	VOLCENGINE_BASE_URL,
	VOLCENGINE_MODEL,
	type SubswapAccount,
} from "../src/subswap.ts";
import {
	VOLCENGINE_MODELS,
	volcengineModel,
	volcengineModelSpec,
} from "../src/volcengine-models.ts";

function account(overrides: Partial<SubswapAccount> & { id: string }): SubswapAccount {
	return {
		provider: "volcengine",
		displayLabel: overrides.id,
		active: false,
		manualOnly: true,
		channel: DEFAULT_VOLCENGINE_CHANNEL,
		...overrides,
	};
}

test("Ark Coding Plan catalog carries the verified model table", () => {
	assert.equal(VOLCENGINE_MODELS.length, 13);
	assert.equal(new Set(VOLCENGINE_MODELS.map((spec) => spec.id)).size, VOLCENGINE_MODELS.length);

	const flash = volcengineModelSpec("deepseek-v4-1-flash");
	assert.ok(flash);
	assert.equal(flash.name, "DeepSeek V4.1 Flash");
	assert.equal(flash.contextWindow, 1_000_000);
	assert.equal(flash.maxTokens, 384_000);
	assert.deepEqual(flash.input, ["text", "image"]);

	const kimi = volcengineModelSpec("kimi-k3.1-pro");
	assert.ok(kimi);
	assert.equal(kimi.name, "Kimi K3.1 Pro");
	assert.equal(kimi.contextWindow, 1_000_000);
	assert.equal(kimi.maxTokens, 128_000);
	assert.deepEqual(kimi.input, ["text", "image"]);

	const textOnly = volcengineModelSpec("glm-5.3");
	assert.ok(textOnly);
	assert.deepEqual(textOnly.input, ["text"]);

	// 别名（kimi-k2.6 / kimi-k2.7-code）与自动路由（ark-code-latest）都不是目录条目。
	assert.equal(volcengineModelSpec("kimi-k2.6"), undefined);
	assert.equal(volcengineModelSpec("kimi-k2.7-code"), undefined);
	assert.equal(volcengineModelSpec("ark-code-latest"), undefined);

	const built = volcengineModel("subswap-volcengine", VOLCENGINE_BASE_URL, kimi);
	assert.equal(built.id, "kimi-k3.1-pro");
	assert.equal(built.api, "openai-completions");
	assert.equal(built.provider, "subswap-volcengine");
	assert.equal(built.baseUrl, VOLCENGINE_BASE_URL);
	assert.equal(built.reasoning, true);
	assert.deepEqual(built.thinkingLevelMap, { xhigh: "xhigh", max: "max" });
});

test("standard Ark endpoint exposes the whole catalog under the unchanged provider id", () => {
	const pool = buildPool({
		channel: DEFAULT_VOLCENGINE_CHANNEL,
		accounts: [account({ id: "ark", model: "kimi-k3.1-pro" })],
		state: newPoolState(),
		resolveSecret: async () => ({ apiKey: "key" }),
	});
	assert.equal(pool.id, "subswap-volcengine");
	assert.equal(pool.id, providerIdForChannel(DEFAULT_VOLCENGINE_CHANNEL));

	const models = pool.getModels();
	assert.equal(models.length, VOLCENGINE_MODELS.length);
	assert.deepEqual(models.map((model) => model.id), VOLCENGINE_MODELS.map((spec) => spec.id));

	const flash = models.find((model) => model.id === "deepseek-v4-1-flash");
	assert.ok(flash);
	assert.equal(flash.name, "DeepSeek V4.1 Flash (Subswap)");
	assert.equal(flash.contextWindow, 1_000_000);
	assert.equal(flash.maxTokens, 384_000);
	assert.deepEqual(flash.input, ["text", "image"]);
	assert.equal(flash.baseUrl, VOLCENGINE_BASE_URL);
	assert.equal(flash.api, "openai-completions");

	const kimi = models.find((model) => model.id === VOLCENGINE_MODEL);
	assert.ok(kimi);
	assert.equal(kimi.name, "Kimi K3.1 Pro (Subswap)");
	assert.equal(kimi.contextWindow, 1_000_000);
	assert.equal(kimi.maxTokens, 128_000);
	assert.deepEqual(kimi.input, ["text", "image"]);
});

test("an account declaring an unknown model appends exactly one extra model", () => {
	const pool = buildPool({
		channel: DEFAULT_VOLCENGINE_CHANNEL,
		accounts: [
			account({ id: "ark-a", active: true, model: "ark-code-latest" }),
			account({ id: "ark-b", model: "ark-code-latest" }),
			account({ id: "ark-c", model: "kimi-k3.1-pro" }),
			account({ id: "ark-d" }),
		],
		state: newPoolState(),
		resolveSecret: async () => ({ apiKey: "key" }),
	});
	const ids = pool.getModels().map((model) => model.id);
	assert.equal(ids.length, VOLCENGINE_MODELS.length + 1);
	assert.equal(new Set(ids).size, ids.length);
	// 目录顺序在前，账号声明的额外模型追加在末尾。
	assert.deepEqual(ids.slice(0, VOLCENGINE_MODELS.length), VOLCENGINE_MODELS.map((spec) => spec.id));

	const extra = pool.getModels().find((model) => model.id === "ark-code-latest");
	assert.ok(extra);
	// 非目录 id 保持旧行为：展示名就是原始 id。
	assert.equal(extra.name, "ark-code-latest (Subswap)");
	assert.equal(extra.baseUrl, VOLCENGINE_BASE_URL);
	assert.equal(extra.api, "openai-completions");
	assert.equal(extra.contextWindow, 1_024_000);
	assert.equal(extra.maxTokens, 1_024_000);
});

test("a custom endpoint channel still exposes exactly one model and keeps its slug id", () => {
	const channel = "volcengine:https://ark.example/api/coding#other-model";
	const pool = buildPool({
		channel,
		accounts: [account({
			id: "ark",
			channel,
			endpoint: "https://ark.example/api/coding",
			model: "other-model",
		})],
		state: newPoolState(),
		resolveSecret: async () => ({ apiKey: "key" }),
	});
	assert.equal(pool.id, providerIdForChannel(channel));
	assert.ok(pool.id.startsWith("subswap-volcengine-"));
	const models = pool.getModels();
	assert.equal(models.length, 1);
	const only = models[0];
	assert.ok(only);
	assert.equal(only.id, "other-model");
	// Anthropic 兼容基址归一化到 OpenAI 兼容的 /v3 路径。
	assert.equal(only.baseUrl, "https://ark.example/api/coding/v3");

	// 自定义中转即使声明一个目录 id，也不会展开目录。
	const relayChannel = "volcengine:https://relay.example/v1#deepseek-v4-1-flash";
	const relay = buildPool({
		channel: relayChannel,
		accounts: [account({
			id: "relay",
			channel: relayChannel,
			endpoint: "https://relay.example/v1",
			model: "deepseek-v4-1-flash",
		})],
		state: newPoolState(),
		resolveSecret: async () => ({ apiKey: "key" }),
	});
	assert.equal(relay.getModels().length, 1);
	assert.equal(relay.getModels()[0]?.id, "deepseek-v4-1-flash");
	assert.equal(relay.getModels()[0]?.baseUrl, "https://relay.example/v1");
});

test("per-attempt resolution keeps the requested model across failover and applies the endpoint override", async () => {
	const seen: Array<{ model: unknown; auth: string | undefined }> = [];
	const server = createServer((req, res) => {
		const chunks: Buffer[] = [];
		req.on("data", (chunk: Buffer) => chunks.push(chunk));
		req.on("end", () => {
			const body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as { model?: unknown };
			seen.push({ model: body.model, auth: req.headers.authorization });
			if (req.headers.authorization === "Bearer key-a") {
				res.writeHead(429, { "content-type": "application/json" });
				res.end(JSON.stringify({
					error: { message: "Coding plan usage limit exceeded", type: "rate_limit_error" },
				}));
				return;
			}
			res.writeHead(200, { "content-type": "text/event-stream; charset=utf-8" });
			res.write(
				'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-v4-1-flash","choices":[{"index":0,"delta":{"role":"assistant","content":"recovered-from-ark-pool"},"finish_reason":null}]}\n\n',
			);
			res.write(
				'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"deepseek-v4-1-flash","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":3,"total_tokens":7}}\n\n',
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
			// 首个账号不带端点：通道仍按标准 Ark 端点建池，目录完整暴露。
			account({ id: "ark-a", displayLabel: "ark …0001", active: true, model: "kimi-k3.1-pro" }),
			// 第二个账号自带端点：验证账号端点覆盖分支同样保留请求模型。
			account({ id: "ark-b", displayLabel: "ark …0002", model: "kimi-k3.1-pro", endpoint: mockRoot }),
		];
		const pool = buildPool({
			channel: DEFAULT_VOLCENGINE_CHANNEL,
			accounts,
			state: newPoolState(),
			resolveSecret: async (acct) => ({
				apiKey: acct.id === "ark-a" ? "key-a" : "key-b",
				endpoint: mockRoot,
			}),
		});
		const requested = pool.getModels().find((model) => model.id === "deepseek-v4-1-flash");
		assert.ok(requested, "expected deepseek-v4-1-flash in the catalog");

		const context = normalizeContext({
			messages: [{ role: "user", content: "Reply with recovered-from-ark-pool", timestamp: Date.now() }],
		});
		const result = await pool.streamSimple(requested, context, {}).result();
		assert.equal(result.stopReason, "stop", `unexpected error: ${result.errorMessage}`);
		const text = result.content
			.filter((part): part is { type: "text"; text: string } => part.type === "text")
			.map((part) => part.text)
			.join("");
		assert.equal(text, "recovered-from-ark-pool");
		// 账号自带 `kimi-k3.1-pro`，请求模型仍是 deepseek-v4-1-flash，两次尝试都不变。
		assert.deepEqual(seen.map((entry) => entry.model), ["deepseek-v4-1-flash", "deepseek-v4-1-flash"]);
		assert.deepEqual(seen.map((entry) => entry.auth), ["Bearer key-a", "Bearer key-b"]);
	} finally {
		await new Promise<void>((resolve, reject) => server.close((err) => (err ? reject(err) : resolve())));
	}
});

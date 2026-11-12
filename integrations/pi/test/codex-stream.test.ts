import assert from "node:assert/strict";
import { test } from "node:test";
import { createModels, normalizeContext } from "@earendil-works/pi-ai";
import { newPoolState } from "../src/pool.ts";
import { buildPool } from "../src/providers.ts";
import type { SubswapAccount } from "../src/subswap.ts";

const accounts: SubswapAccount[] = [
	{ id: "a", provider: "codex", displayLabel: "Account A", active: true, manualOnly: false },
	{ id: "b", provider: "codex", displayLabel: "Account B", active: false, manualOnly: false },
];

// 只供本地假 fetch 解析身份；不含有效签名，不可向真实服务认证。
function fixtureToken(accountId: string): string {
	const payload = Buffer.from(JSON.stringify({
		"https://api.openai.com/auth": { chatgpt_account_id: accountId },
	})).toString("base64url");
	return `fixture.${payload}.unsigned`;
}

for (const errorType of ["error", "response.failed"]) {
	test(`native Codex ${errorType} over HTTP 200 fails over after normalizing the quota code`, async () => {
		const state = newPoolState();
		const provider = buildPool({
			subswapProvider: "codex",
			accounts,
			state,
			resolveSecret: async (account) => ({ apiKey: fixtureToken(account.id) }),
		});
		const registry = createModels();
		registry.setProvider(provider);
		const model = registry.getModel("subswap-codex", "gpt-6.1-sol");
		assert.ok(model);
		const attempts: string[] = [];
		const requests: unknown[] = [];
		const failures: string[] = [];
		const statuses: number[] = [];
		const error = { code: "usage_limit_reached", message: "The usage limit has been reached" };
		const context = normalizeContext({ messages: [{ role: "user", content: "Original task", timestamp: 1 }] });
		const stream = registry.streamSimple(model, context, {
			transport: "sse",
			reasoning: "xhigh",
			onPayload: (payload) => { requests.push(structuredClone(payload)); },
			fetch: async (_url, init) => {
				const id = new Headers(init?.headers).get("chatgpt-account-id");
				assert.ok(id);
				attempts.push(id);
				const events = id === "a"
					? [errorType === "error" ? { type: errorType, ...error } : { type: errorType, response: { error } }]
					: [{
						type: "response.output_item.added", output_index: 0,
						item: { type: "message", id: "fixture-message", role: "assistant", content: [] },
					}, {
						type: "response.output_text.delta", output_index: 0, content_index: 0, delta: "continued",
					}, {
						type: "response.completed",
						response: {
							id: "fixture-response", status: "completed",
							output: [{
								type: "message", id: "fixture-message", role: "assistant",
								content: [{ type: "output_text", text: "continued", annotations: [] }],
							}],
						},
					}];
				return new Response(events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join(""), {
					status: 200, headers: { "content-type": "text/event-stream" },
				});
			},
			onResponse: (response) => { statuses.push(response.status); },
		});
		for await (const event of stream) {
			if (event.type === "error") failures.push(event.error.errorMessage ?? "");
		}
		const result = await stream.result();
		assert.equal(result.errorMessage, undefined);
		assert.equal(result.stopReason, "stop");
		assert.equal(result.content.find((block) => block.type === "text")?.text, "continued");
		assert.deepEqual(attempts, ["a", "b"]);
		assert.deepEqual(statuses, [200, 200]);
		assert.deepEqual(failures, []);
		assert.deepEqual(requests[0], requests[1]);
		assert.equal(state.lastHealthyId, "b");
		assert.ok(state.cooldowns.has("a"));
	});
}

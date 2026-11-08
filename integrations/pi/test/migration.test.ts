import assert from "node:assert/strict";
import { test } from "node:test";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import registerExtension from "../src/index.ts";
import { legacyPoolFor } from "../src/migration.ts";

test("removed coding-plan aliases map to the Volcengine pool", () => {
	assert.equal(legacyPoolFor("coding-plan"), "subswap-volcengine");
	assert.equal(legacyPoolFor("coding-plan-2"), "subswap-volcengine");
});

test("removed Codex aliases map to the Codex pool", () => {
	assert.equal(legacyPoolFor("codex-1"), "subswap-codex");
	assert.equal(legacyPoolFor("codex-2"), "subswap-codex");
});

test("current providers and unknown providers are left alone", () => {
	assert.equal(legacyPoolFor("subswap-codex"), undefined);
	assert.equal(legacyPoolFor("subswap-volcengine"), undefined);
	assert.equal(legacyPoolFor("deepseek"), undefined);
	assert.equal(legacyPoolFor("anthropic"), undefined);
});

type SessionStartHandler = (
	event: unknown,
	ctx: {
		model?: { provider: string; id: string };
		modelRegistry: { find: (provider: string, id: string) => unknown };
	},
) => Promise<void>;

/** Load the extension with a stubbed Pi API and capture its session_start handler. */
async function captureSessionStart(setModel: (model: unknown) => Promise<boolean>): Promise<{
	handler: SessionStartHandler;
	setModelCalls: unknown[];
}> {
	const setModelCalls: unknown[] = [];
	const handlers = new Map<string, SessionStartHandler>();
	const previousBin = process.env.SUBSWAP_BIN;
	// Point at a binary that cannot exist so startup discovery fails closed
	// without touching the real Subswap installation.
	process.env.SUBSWAP_BIN = "/nonexistent-subswap-bin-for-tests";
	try {
		await registerExtension({
			registerProvider: () => {},
			on: (event: string, handler: SessionStartHandler) => {
				handlers.set(event, handler);
				return () => {};
			},
			setModel: async (model: unknown) => {
				setModelCalls.push(model);
				return setModel(model);
			},
		} as unknown as ExtensionAPI);
	} finally {
		if (previousBin === undefined) delete process.env.SUBSWAP_BIN;
		else process.env.SUBSWAP_BIN = previousBin;
	}
	const handler = handlers.get("session_start");
	assert.ok(handler, "extension registers a session_start handler");
	return { handler, setModelCalls };
}

function legacyContext(provider: string, id = "model-id") {
	const migrated = { provider: "subswap-volcengine", id };
	return {
		ctx: {
			model: { provider, id },
			modelRegistry: { find: () => migrated },
		},
		migrated,
	};
}

test("legacy alias sessions migrate silently with the model-memory suppression marker", async () => {
	const { ctx, migrated } = legacyContext("coding-plan");
	let markerAtSetModel: string | undefined;
	const before = Date.now();
	delete process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL;
	const { handler, setModelCalls } = await captureSessionStart(async () => {
		markerAtSetModel = process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL;
		return true;
	});
	try {
		await handler({}, ctx);
	} finally {
		delete process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL;
	}
	assert.deepEqual(setModelCalls, [migrated]);
	assert.ok(markerAtSetModel, "suppression marker is set before setModel runs");
	assert.ok(Number(markerAtSetModel) > before, "suppression marker extends into the future");
});

test("a failing setModel keeps the prior selection without surfacing an error", async () => {
	const { ctx } = legacyContext("codex-1");
	for (const setModel of [
		async () => { throw new Error("No API key"); },
		async () => false,
	]) {
		const { handler, setModelCalls } = await captureSessionStart(setModel);
		delete process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL;
		try {
			await handler({}, ctx);
		} finally {
			delete process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL;
		}
		assert.equal(setModelCalls.length, 1);
	}
});

test("a failing model lookup never surfaces an extension error", async () => {
	const { handler, setModelCalls } = await captureSessionStart(async () => true);
	const throwingCtx = {
		model: { provider: "coding-plan", id: "model-id" },
		modelRegistry: {
			find: () => { throw new Error("registry unavailable"); },
		},
	};
	delete process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL;
	try {
		await handler({}, throwingCtx);
		await handler({}, { model: undefined, modelRegistry: throwingCtx.modelRegistry });
		await handler({}, {
			model: { provider: "anthropic", id: "model-id" },
			modelRegistry: { find: () => { throw new Error("must not be called"); } },
		});
	} finally {
		delete process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL;
	}
	assert.equal(setModelCalls.length, 0);
});

test("an unmigrated lookup result leaves the current model alone", async () => {
	const { handler, setModelCalls } = await captureSessionStart(async () => true);
	const ctx = {
		model: { provider: "coding-plan", id: "model-id" },
		modelRegistry: { find: () => undefined },
	};
	delete process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL;
	try {
		await handler({}, ctx);
	} finally {
		delete process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL;
	}
	assert.equal(setModelCalls.length, 0);
});

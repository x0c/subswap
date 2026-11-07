import assert from "node:assert/strict";
import { test } from "node:test";
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

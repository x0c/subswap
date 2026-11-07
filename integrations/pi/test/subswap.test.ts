import assert from "node:assert/strict";
import { test } from "node:test";
import { parseAuth, parseStatus, SubswapClient, type Exec } from "../src/subswap.ts";

test("status discovery preserves provider groups and manual-only metadata", () => {
	const accounts = parseStatus({
		accounts: [
			{ id: "a@example.com", provider: "codex", display_label: "A", active: true, manual_only: false },
			{ id: "ark-one", provider: "volcengine", display_label: "Ark one", active: true, manual_only: true },
			{ id: "ark-two", provider: "volcengine", display_label: "Ark two", active: false, manual_only: true },
		],
	});
	assert.deepEqual(accounts.map((account) => account.provider), ["codex", "volcengine", "volcengine"]);
	assert.equal(accounts[2]?.manualOnly, true);
});

test("auth parsing accepts only an access credential or supported environment key", () => {
	const credential = parseAuth({ kind: "credential", secret: "access-only", expires_at_ms: 100 });
	assert.deepEqual(credential, { apiKey: "access-only", expiresAtMs: 100 });
	assert.equal(JSON.stringify(credential).includes("refresh"), false);
	const environment = parseAuth({
		kind: "environment",
		env: { ANTHROPIC_AUTH_TOKEN: "ark-key", OTHER: "ignored" },
	});
	assert.deepEqual(environment, { apiKey: "ark-key" });
	assert.throws(() => parseAuth({ kind: "credential", secret: "" }), /no usable request credential/);
	assert.throws(() => parseAuth({ kind: "environment", env: { UNRELATED: "secret" } }), /no usable request credential/);
});

test("an expired access token uses Subswap renewal and then rereads the same account", async () => {
	const calls: string[][] = [];
	const account = { id: "a@example.com", provider: "codex", displayLabel: "A", active: true, manualOnly: false };
	const exec: Exec = async (args) => {
		calls.push(args);
		if (args[0] === "--json") return { stdout: "{}" };
		if (args[0] === "status") return { stdout: JSON.stringify({ accounts: [] }) };
		const fresh = calls.filter((call) => call[0] === "--json").length > 0;
		return {
			stdout: JSON.stringify({
				kind: "credential",
				secret: fresh ? "fresh-access" : "expired-access",
				expires_at_ms: fresh ? Date.now() + 3_600_000 : Date.now() - 1000,
			}),
		};
	};
	const client = new SubswapClient({ exec });
	const secret = await client.requestSecret(account);
	assert.equal(secret.apiKey, "fresh-access");
	assert.deepEqual(calls, [
		["auth", "codex/a@example.com"],
		["--json"],
		["auth", "codex/a@example.com"],
	]);
});

test("credential projection is reused until a failure invalidates it", async () => {
	const calls: string[][] = [];
	const account = { id: "ark-test", provider: "volcengine", displayLabel: "Ark test", active: true, manualOnly: true };
	const exec: Exec = async (args) => {
		calls.push(args);
		return { stdout: JSON.stringify({ kind: "environment", env: { ANTHROPIC_AUTH_TOKEN: "ark-key" } }) };
	};
	const client = new SubswapClient({ exec });
	assert.equal((await client.requestSecret(account)).apiKey, "ark-key");
	assert.equal((await client.requestSecret(account)).apiKey, "ark-key");
	assert.equal(calls.length, 1);
	client.invalidate(account);
	assert.equal((await client.requestSecret(account)).apiKey, "ark-key");
	assert.equal(calls.length, 2);
});

test("missing or malformed credential errors never echo provider output", async () => {
	const exec: Exec = async () => { throw new Error("Subswap auth failed"); };
	const client = new SubswapClient({ exec });
	const account = { id: "ark-test", provider: "volcengine", displayLabel: "Ark test", active: true, manualOnly: true };
	await assert.rejects(client.requestSecret(account), (error: Error) => {
		assert.equal(error.message, "Subswap auth failed");
		assert.equal(error.message.includes("secret"), false);
		return true;
	});
});

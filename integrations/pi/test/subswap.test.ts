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

test("an expired access token uses per-account renewal and then rereads the same account", async () => {
	const calls: string[][] = [];
	const account = { id: "a@example.com", provider: "codex", displayLabel: "A", active: true, manualOnly: false };
	const exec: Exec = async (args) => {
		calls.push(args);
		if (args[0] === "status") return { stdout: JSON.stringify({ accounts: [] }) };
		const renewed = calls.some((call) => call[2] === "--renew");
		return {
			stdout: JSON.stringify({
				kind: "credential",
				secret: renewed ? "fresh-access" : "expired-access",
				expires_at_ms: renewed ? Date.now() + 3_600_000 : Date.now() - 1000,
			}),
		};
	};
	const client = new SubswapClient({ exec });
	const secret = await client.requestSecret(account);
	assert.equal(secret.apiKey, "fresh-access");
	assert.deepEqual(calls, [
		["auth", "codex/a@example.com"],
		["auth", "codex/a@example.com", "--renew"],
		["auth", "codex/a@example.com"],
	]);
	assert.equal(calls.some((call) => call.length === 1 && call[0] === "--json"), false);
});

test("concurrent expiry on different accounts renews each account separately", async () => {
	const calls: string[][] = [];
	const accountA = { id: "a@example.com", provider: "codex", displayLabel: "A", active: true, manualOnly: false };
	const accountB = { id: "b@example.com", provider: "codex", displayLabel: "B", active: true, manualOnly: false };
	const exec: Exec = async (args) => {
		calls.push(args);
		if (args[0] === "status") return { stdout: JSON.stringify({ accounts: [] }) };
		if (args[2] === "--renew") {
			await new Promise((resolve) => setTimeout(resolve, 20));
			return { stdout: "{}" };
		}
		const key = args[1] ?? "";
		const renewed = calls.some((call) => call[1] === key && call[2] === "--renew");
		return {
			stdout: JSON.stringify({
				kind: "credential",
				secret: renewed ? `fresh-${key}` : `expired-${key}`,
				expires_at_ms: renewed ? Date.now() + 3_600_000 : Date.now() - 1000,
			}),
		};
	};
	const client = new SubswapClient({ exec });
	const [secretA, secretB] = await Promise.all([
		client.requestSecret(accountA),
		client.requestSecret(accountB),
	]);
	assert.equal(secretA.apiKey, "fresh-codex/a@example.com");
	assert.equal(secretB.apiKey, "fresh-codex/b@example.com");
	const renewals = calls.filter((call) => call[2] === "--renew");
	assert.deepEqual(renewals, [
		["auth", "codex/a@example.com", "--renew"],
		["auth", "codex/b@example.com", "--renew"],
	]);
});

test("concurrent expiry on the same account shares one in-flight renewal", async () => {
	let renewCalls = 0;
	const account = { id: "a@example.com", provider: "codex", displayLabel: "A", active: true, manualOnly: false };
	const exec: Exec = async (args) => {
		if (args[2] === "--renew") {
			renewCalls += 1;
			await new Promise((resolve) => setTimeout(resolve, 20));
			return { stdout: "{}" };
		}
		return {
			stdout: JSON.stringify({
				kind: "credential",
				secret: renewCalls > 0 ? "fresh-access" : "expired-access",
				expires_at_ms: renewCalls > 0 ? Date.now() + 3_600_000 : Date.now() - 1000,
			}),
		};
	};
	const client = new SubswapClient({ exec });
	const [first, second] = await Promise.all([
		client.requestSecret(account),
		client.requestSecret(account),
	]);
	assert.equal(first.apiKey, "fresh-access");
	assert.equal(second.apiKey, "fresh-access");
	assert.equal(renewCalls, 1);
});

test("a renewal skipped by the anti-storm throttle stays fail-closed", async () => {
	const calls: string[][] = [];
	const account = { id: "a@example.com", provider: "codex", displayLabel: "A", active: true, manualOnly: false };
	const exec: Exec = async (args) => {
		calls.push(args);
		if (args[2] === "--renew") return { stdout: "{}" };
		return {
			stdout: JSON.stringify({
				kind: "credential",
				secret: "expired-access",
				expires_at_ms: Date.now() - 1000,
			}),
		};
	};
	const client = new SubswapClient({ exec });
	await assert.rejects(client.requestSecret(account), /needs a fresh login/);
	const renewalsAfterFirst = calls.filter((call) => call[2] === "--renew").length;
	assert.equal(renewalsAfterFirst, 1);
	client.invalidate(account);
	await assert.rejects(client.requestSecret(account), /needs a fresh login/);
	assert.equal(calls.filter((call) => call[2] === "--renew").length, 1);
});

test("renewal forwards the abort signal with a bounded timeout", async () => {
	let seenSignal: AbortSignal | undefined;
	let seenTimeout = 0;
	const account = { id: "a@example.com", provider: "codex", displayLabel: "A", active: true, manualOnly: false };
	const controller = new AbortController();
	const exec: Exec = async (_args, options) => {
		seenSignal = options.signal;
		seenTimeout = options.timeoutMs;
		return { stdout: "{}" };
	};
	const client = new SubswapClient({ exec });
	await client.renew(account, controller.signal);
	assert.equal(seenSignal, controller.signal);
	assert.ok(seenTimeout > 0 && seenTimeout <= 60_000);
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

test("malformed subprocess output throws a generic error without echoing secrets", async () => {
	const leaked = "sk-live-malformed-secret-abc123";
	const malformedAuth: Exec = async () => ({ stdout: `{"kind":"credential","secret":"${leaked}` });
	const authClient = new SubswapClient({ exec: malformedAuth });
	const account = { id: "ark-test", provider: "volcengine", displayLabel: "Ark test", active: true, manualOnly: true };
	await assert.rejects(authClient.requestSecret(account), (error: Error) => {
		assert.equal(error.message, "Subswap returned unreadable output");
		assert.equal(error.message.includes(leaked), false);
		return true;
	});

	const nonObjectAuth: Exec = async () => ({ stdout: `["${leaked}"]` });
	await assert.rejects(new SubswapClient({ exec: nonObjectAuth }).requestSecret(account), (error: Error) => {
		assert.equal(error.message, "Subswap returned unreadable output");
		assert.equal(error.message.includes(leaked), false);
		return true;
	});

	const malformedStatus: Exec = async () => ({ stdout: `{"accounts":[{"id":"${leaked}"` });
	await assert.rejects(new SubswapClient({ exec: malformedStatus }).accounts(), (error: Error) => {
		assert.equal(error.message, "Subswap returned unreadable output");
		assert.equal(error.message.includes(leaked), false);
		return true;
	});
});

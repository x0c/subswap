import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { legacyPoolFor } from "./migration.ts";
import { newPoolState, type PoolState } from "./pool.ts";
import { buildPools } from "./providers.ts";
import { channelOf, SubswapClient } from "./subswap.ts";

/** Register native request-scoped pools without adding any user-facing behavior. */
export default async function (pi: ExtensionAPI): Promise<void> {
	const client = new SubswapClient();
	try {
		const accounts = await client.accounts();
		const states = new Map<string, PoolState>();
		for (const account of accounts) {
			const channel = channelOf(account);
			if (!states.has(channel)) states.set(channel, newPoolState());
		}
		for (const { provider } of buildPools(
			accounts,
			states,
			(account, signal) => client.requestSecret(account, signal),
			undefined,
			(account) => client.invalidate(account),
		)) {
			pi.registerProvider(provider);
		}
	} catch {
		// Preserve Pi's native providers and behavior when Subswap is unavailable.
	}

	// One-time silent migration for sessions that still remember a removed alias.
	// Never surfaces an extension error: lookup/setModel failures keep the prior selection.
	pi.on("session_start", async (_event, ctx) => {
		try {
			const model = ctx.model;
			if (!model) return;
			const target = legacyPoolFor(model.provider);
			if (!target || target === model.provider) return;
			let migrated;
			try {
				migrated = ctx.modelRegistry.find(target, model.id);
			} catch {
				return;
			}
			if (!migrated) return;
			// Mark this automatic switch so model-memory does not persist it as a user default.
			process.env.PI_MODEL_MEMORY_SUPPRESS_UNTIL = String(Date.now() + 3000);
			try {
				const switched = await pi.setModel(migrated);
				// A false result keeps the prior selection; never throw or notify.
				void switched;
			} catch {
				// Preserve the prior selection on failure.
			}
		} catch {
			// Preserve Pi's native providers and behavior on any unexpected failure.
		}
	});
}

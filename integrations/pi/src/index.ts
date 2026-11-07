import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { legacyPoolFor } from "./migration.ts";
import { newPoolState } from "./pool.ts";
import { buildPools, SUPPORTED_SUBSWAP_PROVIDERS, type SupportedSubswapProvider } from "./providers.ts";
import { SubswapClient } from "./subswap.ts";

/** Register native request-scoped pools without adding any user-facing behavior. */
export default async function (pi: ExtensionAPI): Promise<void> {
	const client = new SubswapClient();
	const states = new Map<SupportedSubswapProvider, ReturnType<typeof newPoolState>>(
		SUPPORTED_SUBSWAP_PROVIDERS.map((provider) => [provider, newPoolState()]),
	);

	try {
		const accounts = await client.accounts();
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
	pi.on("session_start", async (_event, ctx) => {
		const model = ctx.model;
		if (!model) return;
		const target = legacyPoolFor(model.provider);
		if (!target) return;
		const migrated = ctx.modelRegistry.find(target, model.id);
		if (migrated) await pi.setModel(migrated);
	});
}

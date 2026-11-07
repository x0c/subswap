/** Old Pi provider ids that the Subswap pools replaced. */
const LEGACY_POOL_BY_PROVIDER: Readonly<Record<string, string>> = {
	"coding-plan": "subswap-volcengine",
	"coding-plan-2": "subswap-volcengine",
	"codex-1": "subswap-codex",
	"codex-2": "subswap-codex",
};

/** Resolve a removed provider id to its same-model Subswap pool. */
export function legacyPoolFor(provider: string): string | undefined {
	return LEGACY_POOL_BY_PROVIDER[provider];
}

import type { Model, ModelCost, ModelInputModality } from "@earendil-works/pi-ai";

/** 目录条目：Ark Coding Plan 单个模型的已验证元数据。 */
export interface VolcengineModelSpec {
	/** 上游模型 id，直接作为请求体 `model` 字段。 */
	id: string;
	/** Pi 模型选择器里的英文展示名。 */
	name: string;
	contextWindow: number;
	maxTokens: number;
	input: readonly ModelInputModality[];
}

/** Subswap 只做账号切换，不做计价；全部 Provider 共用零成本字面量。 */
export function zeroCost(): ModelCost {
	return { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
}

/**
 * Ark Coding Plan 已验证模型目录（2026-11-16 对
 * https://ark.cn-beijing.volces.com/api/coding/v3 实测）。
 *
 * 未收录的 id：`kimi-k2.6` 与 `kimi-k2.7-code` 都是 `kimi-k3.1-pro` 的别名；
 * `ark-code-latest` 是自动路由（返回 `model: "auto"`），都不属于独立模型。
 * 它们仍可作为账号声明模型按旧路径追加，见 `buildPool` 的 volcengine 分支。
 */
export const VOLCENGINE_MODELS: readonly VolcengineModelSpec[] = [
	{ id: "deepseek-v4-1-flash", name: "DeepSeek V4.1 Flash", contextWindow: 1_000_000, maxTokens: 384_000, input: ["text", "image"] },
	{ id: "deepseek-v4-1-flash-economy", name: "DeepSeek V4.1 Flash Economy", contextWindow: 1_000_000, maxTokens: 384_000, input: ["text", "image"] },
	{ id: "deepseek-v4-pro", name: "DeepSeek V4 Pro", contextWindow: 1_000_000, maxTokens: 384_000, input: ["text"] },
	{ id: "deepseek-v4-flash", name: "DeepSeek V4 Flash", contextWindow: 1_000_000, maxTokens: 384_000, input: ["text"] },
	{ id: "kimi-k3.1-pro", name: "Kimi K3.1 Pro", contextWindow: 1_000_000, maxTokens: 128_000, input: ["text", "image"] },
	{ id: "kimi-k3", name: "Kimi K3", contextWindow: 1_000_000, maxTokens: 32_000, input: ["text", "image"] },
	{ id: "kimi-k2-8-preview", name: "Kimi K2.8 Preview", contextWindow: 256_000, maxTokens: 32_000, input: ["text", "image"] },
	{ id: "glm-5.3", name: "GLM-5.3", contextWindow: 1_000_000, maxTokens: 128_000, input: ["text"] },
	{ id: "glm-5-3-flash", name: "GLM-5.3 Flash", contextWindow: 1_000_000, maxTokens: 128_000, input: ["text", "image"] },
	{ id: "doubao-seed-evolving", name: "Doubao Seed Evolving", contextWindow: 1_000_000, maxTokens: 256_000, input: ["text", "image"] },
	{ id: "doubao-seed-2-1-pro", name: "Doubao Seed 2.1 Pro", contextWindow: 1_000_000, maxTokens: 256_000, input: ["text", "image"] },
	{ id: "doubao-seed-2-1-lite", name: "Doubao Seed 2.1 Lite", contextWindow: 1_000_000, maxTokens: 256_000, input: ["text", "image"] },
	{ id: "doubao-seed-2-0-mini", name: "Doubao Seed 2.0 Mini", contextWindow: 256_000, maxTokens: 128_000, input: ["text", "image"] },
];

/** 把目录条目实例化成某个 Pi Provider 下的 `openai-completions` 模型。 */
export function volcengineModel(
	providerId: string,
	endpoint: string,
	spec: VolcengineModelSpec,
): Model<"openai-completions"> {
	return {
		id: spec.id,
		name: spec.name,
		api: "openai-completions",
		provider: providerId,
		baseUrl: endpoint,
		reasoning: true,
		thinkingLevelMap: { xhigh: "xhigh", max: "max" },
		input: [...spec.input],
		cost: zeroCost(),
		contextWindow: spec.contextWindow,
		maxTokens: spec.maxTokens,
	};
}

/** 按模型 id 查目录；别名、自动路由与账号自定义 id 返回 `undefined`。 */
export function volcengineModelSpec(modelId: string): VolcengineModelSpec | undefined {
	return VOLCENGINE_MODELS.find((spec) => spec.id === modelId);
}

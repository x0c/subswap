# Pi 火山渠道只能选到一个模型（2026-11-16）

## Symptoms

1. Pi 的模型选择器里 `subswap-volcengine` 只有一行，通常是 `kimi-k3.1-pro`；方舟控制台里开通的
   DeepSeek / GLM / Doubao 模型一个都不出现。`pi --list-models volcengine` 同样只打印一行。
2. 想加 DeepSeek 时，`subswap add-api pi --preset volcengine --model deepseek-v4-1-flash --api-key <key>`
   之后选择器里仍然只有旧模型（或者干脆多出一个一模一样的账号）。
3. 起点还常带一句 `Warning: No models match pattern "deepseek/*"`，容易被误当成模型缺失的原因。

## Root Cause

1. **扩展按“一个渠道一个模型”注册。** `integrations/pi/src/providers.ts` 的 volcengine 分支取
   `accounts[0].model`（默认 `kimi-k3.1-pro`）后 `models: [volcengineModelFor(...)]`，Pi 只可能看到这一个；
   而 `resolveVolcengineModel` 每次请求又按**当前账号**的 `model` 改写 id，所以即使想办法塞进多模型，
   也会变成「选择器显示一个名字、请求按账号偷换模型」。
2. **`enabledModels` 是第二道过滤。** `~/.pi/agent/settings.json` 里只写了
   `subswap-volcengine/kimi-k3.1-pro`，即便 provider 有更多模型也不会进选择器（`docs/settings.md`：
   该 pattern 决定启动选择与 Ctrl+P 轮换范围）。
3. **channel 字符串不参与模型选择。** `volcengine:<base>#<model>` 里的 `#model` 只拼字符串；真正生效的是
   账号记录里的 `pi_api.model`。所以改 channel 不会加模型，`add-api` 生成的 channel 又恒为默认值，
   同 slot 同 key 的账号 id 还相同（`account_id_for = sha256(slot, key)`），重复登记会覆盖而不是新增。
4. **`deepseek/*` 那条警告无关。** 它属于 Pi 原生 `deepseek` provider（需要 `DEEPSEEK_API_KEY` 或
   `auth.json` 的 `deepseek` slot），与本机火山渠道没有关系。

## Remedy

`subswap-volcengine` 改为暴露方舟 Coding Plan 的完整模型目录（`integrations/pi/src/volcengine-models.ts`，
2026-11-16 实测 13 个：DeepSeek V4.1 Flash / V4.1 Flash Economy / V4 Pro / V4 Flash、Kimi K3.1 Pro / K3 /
K2.8 Preview、GLM-5.3 / GLM-5.3 Flash、Doubao Seed Evolving / 2.1 Pro / 2.1 Lite / 2.0 Mini）：

- provider id 保持 `subswap-volcengine`，已有 `enabledModels` / `modelThinkingLevels` 键不失效；
- 账号声明目录外 id（如 `ark-code-latest`）时自动追加一条；
- `resolveVolcengineModel` 只修端点、不再改写模型 id，补位始终保持用户选的模型；
- 自定义中转端点渠道仍只暴露自己的一个模型（未知目录，不编造）。
- 机器侧把 `enabledModels` 的 `subswap-volcengine/kimi-k3.1-pro` 换成显式两条
  `subswap-volcengine/kimi-k3.1-pro` + `subswap-volcengine/deepseek-v4-1-flash`（不要用 `subswap-volcengine/*`：
  那会把目录里 13 条全放进选择器）。provider 仍保留完整目录，只靠 scope 控制可见集。

## Verification

- `integrations/pi`：`tsc --noEmit` 通过，`npm test` 50/50（新增 `test/volcengine-models.test.ts` 覆盖目录元数据、
  全目录暴露、目录外追加去重、自定义端点单模型、以及本地 HTTP 补位中模型 id 不被改写）。
- 真机 `pi --list-models volcengine`（扩展经 `~/.pi/agent/extensions/subswap-pi` 加载）从 1 行变成 13 行，
  含 `deepseek-v4-1-flash`（1M 上下文 / 384K 输出 / 支持图片）。
- 上游实测：同一批 key 直连 `POST /api/coding/v3/chat/completions` 时每个目录 id 均返回 200，
  `deepseek-v4-1-flash` 还接受 `thinking:{type:"enabled"}` 与 `reasoning_effort` `max`/`xhigh`；
  `kimi-k2.6`、`kimi-k2.7-code` 归一化到 `kimi-k3.1-pro`，`ark-code-latest` 归一化到 `auto` 路由。
- 限制：验收时两条 Ark key 的 5 小时窗口都已耗尽（`AccountQuotaExceeded`），所以逐模型的真实推理留待配额重置后复查；
  模型清单本身来自 Pi 自己的 `--list-models`。

## 顺带确认

`kimi-k2.6` / `kimi-k2.7-code` 是别名不是新模型，`ark-code-latest` 是控制台自动路由；
看到它们「可用」并不等于方舟新增了模型。

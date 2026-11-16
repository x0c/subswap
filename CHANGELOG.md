# Changelog

Release notes are bilingual: English first, then complete Simplified Chinese.
Development adds entries under `## Unreleased`; at release time they become the
next version's section. A `Release: minor` or `Release: major` line requests a
larger version step; `Release: hold <reason>` keeps the changes unreleased until
the named acceptance is done.

## Unreleased

### English

- Expose the whole Ark Coding Plan catalog on the `subswap-volcengine` Pi provider instead of one model: the verified model table lives in `integrations/pi/src/volcengine-models.ts` (DeepSeek V4.1 Flash / V4.1 Flash Economy / V4 Pro / V4 Flash, Kimi K3.1 Pro / K3 / K2.8 Preview, GLM-5.3 / GLM-5.3 Flash, Doubao Seed Evolving / 2.1 Pro / 2.1 Lite / 2.0 Mini), the provider id stays `subswap-volcengine` so existing model scopes keep working, an account declaring an id outside the catalog (for example `ark-code-latest`) appends that id as one extra model, and a custom endpoint channel still exposes only its own model. Pooled failover now keeps the model the user selected in Pi: the per-attempt hook only refreshes the endpoint and never rewrites the model id from account metadata any more.
- Restore Volcengine (火山方舟) Coding Plan quota for the Pi drawer: V4 signing and the `GetCodingPlanUsage` / `GetAFPUsage` calls now live in `providers/common` and are shared with the read-only top-level provider, the per-account quota AK/SK survives the legacy migration (with backfill), and `add-api pi --preset volcengine` / `login volcengine` accept `--ak-id` / `--ak-secret`. Accounts without the pair still show no windows instead of fabricated numbers.

### 简体中文

- `subswap-volcengine` 从「一个账号模型」改为暴露方舟 Coding Plan 的完整模型目录：已验证的模型表放在 `integrations/pi/src/volcengine-models.ts`（DeepSeek V4.1 Flash / V4.1 Flash Economy / V4 Pro / V4 Flash、Kimi K3.1 Pro / K3 / K2.8 Preview、GLM-5.3 / GLM-5.3 Flash、Doubao Seed Evolving / 2.1 Pro / 2.1 Lite / 2.0 Mini），provider id 仍是 `subswap-volcengine`，既有模型 scope 不失效；账号声明目录外的 id（如 `ark-code-latest`）会自动追加一条，自定义中转端点渠道仍只暴露自己的模型。池内补位改为保持用户在 Pi 里选的模型：单次尝试的 hook 只修端点，不再按账号 `model` 改写模型 id。
- 恢复 Pi 抽屉里火山方舟 Coding Plan 的余量查询：V4 签名与 `GetCodingPlanUsage` / `GetAFPUsage` 调用移到 `providers/common`，与只读的顶层 provider 共用一套实现；账号级额度 AK/SK 在旧版迁移中保留（并可从残留回补）；`add-api pi --preset volcengine` / `login volcengine` 支持 `--ak-id` / `--ak-secret`。缺这对凭据时仍显示无窗口，不编造数字。

## v1.17.1

### English

- Re-release the v1.17.0 changes with the macOS release builds moved to the supported `macos-15` runner image. GitHub retired `macos-14`, which cancelled both macOS builds of the v1.17.0 draft; no source or behavior changed.

### 简体中文

- 重新发布 v1.17.0 的改动，macOS 发布构建改用仍受支持的 `macos-15` 运行器镜像。GitHub 已下线 `macos-14`，导致 v1.17.0 草稿的两个 macOS 构建被取消；本次没有源码或行为变化。

Full release notes, verification and limits: [docs/releases/v1.17.1.md](docs/releases/v1.17.1.md).

## v1.17.0

### English

- Add a `pi` provider for Pi's multi-slot authentication drawer. `subswap login pi` imports live slots and installs the optional Pi extension, `subswap add-api pi` adds a key, and `swap`, `rm`, `run`, `shell`, `env` and `auth` accept `pi/<id>`. `swap --no-default` changes only the live slot without changing Pi's default selection, and removing an account deletes only that slot.
- Add the optional `integrations/pi` extension (`subswap login pi` or `make install-pi`). It maps each Subswap channel to one Pi provider (`subswap-codex`, `subswap-volcengine`, `subswap-opencode-go`, `subswap-opencode-zen`, plus custom Codex and Volcengine channels) and fails over between that channel's accounts inside a single request, preserving the model, thinking level, conversation and task. It reads request-scoped credentials through `subswap auth`, never receives refresh tokens, and adds no prompts, commands or notifications.
- Import Volcengine Ark Coding Plan keys into the Pi drawer with `subswap login volcengine -- <key>` (alias `ark`) or `subswap add-api pi --preset volcengine`. Existing top-level Volcengine accounts migrate into `pi/volcengine` on first run; the retired AK/SK quota lookup is dropped because Pi accounts are flat-rate.
- Add an Antigravity (`agy`) provider for the macOS keychain login. The default view, `status` and `status --json` show the independent Gemini and Claude/GPT pools on separate rows, with same-name windows disambiguated in JSON (`gemini_5h`, `claude_5h`, ...); automatic switching considers every pool, and external native logins are imported before quota collection.
- Extend `add-api` to choose Claude Code, Codex or Pi first. Codex custom channels use the official `[model_providers.subswap]` entry with a command-backed auth helper, so the key stays in Subswap's credential store and out of `config.toml`; switching back to OAuth or deleting the active channel restores the managed `model`/`model_provider`. Deleting an active custom channel is no longer refused, and `--json add-api` returns one structured receipt.
- Add `subswap auth <id|N>`, a trusted local consumer projection that writes exactly one JSON object with the request-scoped access token or API key (`kind=credential`) or its environment mapping (`kind=environment`). It never returns refresh tokens, stays read-only apart from an optional `--renew` that renews only that account near expiry, and supports `--raw` for Codex's auth helper.
- Add `status --refresh` for explicit client-side refreshes; the default `status` remains a read-only snapshot. `subswap --json swap <target>` returns a structured receipt (`provider`, `id`, optional `notice`), and `subswap <id|N>` switches directly, matching `subswap swap <id|N>`.
- Fix quota rendering for accounts without quota: an empty successful reading is a valid cache hit, so such accounts show `custom` instead of a repeated quota-fetch error. Add integration coverage for credential projection and structured swaps, refresh the app icon and READMEs, and document the JSON IPC, MSRV 1.80 and request-failover contracts.

### 简体中文

- 新增 `pi` Provider，管理 Pi 的多槽位认证文件。`subswap login pi` 导入 live 槽位并安装可选 Pi 扩展，`subswap add-api pi` 添加 Key，`swap`、`rm`、`run`、`shell`、`env`、`auth` 均接受 `pi/<id>`。`swap --no-default` 只改写 live 槽位、不改 Pi 的默认选择；删除账号只清对应槽位。
- 新增可选 `integrations/pi` 扩展（`subswap login pi` 或 `make install-pi` 安装）。它把每个 Subswap 渠道映射为一个 Pi Provider（`subswap-codex`、`subswap-volcengine`、`subswap-opencode-go`、`subswap-opencode-zen`，以及 Codex / Volcengine 自定义渠道），在同一次请求内于该渠道的账号间补位，保持模型、思考等级、会话与任务不变。它经 `subswap auth` 按请求读取凭证，永远拿不到 refresh token，也不注入提示词、命令或通知。
- 火山方舟 Coding Plan Key 改为用 `subswap login volcengine -- <key>`（别名 `ark`）或 `subswap add-api pi --preset volcengine` 导入 Pi 抽屉。既有顶层火山账号在首次运行后迁移到 `pi/volcengine`；顶层渠道退役后不再提供 AK/SK 查额度，Pi 账号按固定订阅展示。
- 新增 Antigravity（`agy`）Provider，导入 macOS 钥匙串登录。默认入口、`status` 与 `status --json` 把相互独立的 Gemini 与 Claude/GPT 额度池分行显示，同名窗口在 JSON 中消歧（`gemini_5h`、`claude_5h` 等）；自动换号按各池判断，采额度前会先把原生客户端外部新登录的账号入池。
- `add-api` 改为先选择 Claude Code、Codex 或 Pi。Codex 自定义渠道写入官方 `[model_providers.subswap]`，用 command 形式的 auth helper 取 Key，Key 只存 Subswap 凭证仓库、不写 `config.toml`；切回 OAuth 或删除当前渠道时恢复受管 `model`/`model_provider`。删除 active 自定义渠道不再被拒绝，`--json add-api` 只输出一份结构化回执。
- 新增 `subswap auth <id|N>`：面向受信本机消费者的凭证投影，恰好输出一个 JSON 对象，包含当前请求所需的 access token / API Key（`kind=credential`）或其环境变量映射（`kind=environment`）。绝不返回 refresh token；除 `--renew` 在临近过期时只续期该账号外全程只读，并支持给 Codex auth helper 用的 `--raw`。
- 新增 `status --refresh` 供客户端显式刷新；默认 `status` 仍是只读快照。`subswap --json swap <目标>` 返回结构化回执（`provider`、`id`、可选 `notice`），`subswap <id|N>` 可直接切换，语义与 `subswap swap <id|N>` 相同。
- 修复无额度账号的额度显示：空成功结果视为有效缓存命中，这类账号显示 `custom`，不再反复显示额度查询错误。新增凭证投影与结构化切换的集成测试，更新应用图标与各语言 README，并记录 JSON IPC、MSRV 1.80 与请求补位契约。

Full release notes, verification and limits: [docs/releases/v1.17.0.md](docs/releases/v1.17.0.md).

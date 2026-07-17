# Kimi 接入 + 文件型 OAuth 账号切换共享引擎 设计

> 状态：设计已确认，待评审 → 进入实现计划。
> 目标：给 subswap 加 Kimi（Moonshot）Provider，并把 Codex 与 Kimi 共有的「文件型 OAuth 账号切换」机制抽成共享引擎，未来接新 agent runtime 只写一个薄适配器。

## 1. 背景与调研结论（实测）

Kimi Code CLI（`@moonshot-ai/kimi-code`）从本机文件读凭证，形态与 Codex 高度一致：

- **凭证文件**：`~/.kimi-code/credentials/kimi-code.json`，目录可用 `KIMI_CODE_HOME` 重定向。
  内含 `access_token` / `refresh_token` / `expires_at` / `scope` / `token_type` / `expires_in`。
- **令牌**：access token 为 JWT，15 分钟过期；refresh token 有效期约 30 天，**单次轮换**（用一次即失效，和 Codex 同款风险）。
- **身份**：JWT payload 里 `user_id`（无 email）、`client_id`、`scope`。account 主键取 `user_id`，label 缺省用 `user_id`（可 `--label` 覆盖）。
- **不碰 macOS 钥匙串**：纯文件切换（比 Claude 简单）。
- **刷新端点**：`POST https://auth.kimi.com/api/oauth/token`
  - `Content-Type: application/x-www-form-urlencoded`，`Accept: application/json`
  - body：`client_id=<id>&grant_type=refresh_token&refresh_token=<token>`
  - `401/403/invalid_grant` → 死 token（needs re-login）
  - 成功返回 `access_token`/`refresh_token`(轮换)/`expires_in`/`scope`/`token_type`
  - host 可用 `KIMI_CODE_OAUTH_HOST` 覆盖（供测试）。
- **额度端点**：`GET https://api.kimi.com/coding/v1/usages`（`Authorization: Bearer`，带 `User-Agent`）。
  base 可用 `KIMI_CODE_BASE_URL` 覆盖。实测返回：

  ```jsonc
  {
    "user": { "userId": "…", "membership": { "level": "LEVEL_INTERMEDIATE" }, "region": "REGION_CN" },
    "usage":  { "limit": "100", "used": "4",  "remaining": "96", "resetTime": "2026-07-24T07:52:15Z" }, // 周窗口(7d)
    "limits": [ { "window": { "duration": 300, "timeUnit": "TIME_UNIT_MINUTE" },
                  "detail": { "limit": "100", "used": "18", "remaining": "82", "resetTime": "2026-07-17T12:52:15Z" } } ], // 5h 窗口
    "boosterWallet": { "balance": { "type": "BOOSTER", "amount": … }, "monthlyChargeLimit": {…} } // 余额,可选
  }
  ```
  - 数值是**字符串**，需转整数；`resetTime` 是 ISO8601。
  - `usage` → `QuotaWindow::SevenDay`；`limits[]` 里 `duration:300 TIME_UNIT_MINUTE` → `QuotaWindow::FiveHour`（一般用规则：`duration + timeUnit` 换算分钟后映射 300→5h、10080→7d，其余 `Custom`）。
  - `boosterWallet` 余额本期不进 quota 百分比，仅可留作展示扩展（YAGNI：先不做）。

## 2. 已确认的产品决策

1. **完整对齐**：Kimi 与 Codex/Claude 同级——添加/导入、手动切换、默认入口显示、`subswap run kimi` 隔离运行、额度显示。
2. **参与自动换号**：Kimi 用 5h 窗口作为自动切换判据，阈值走 `AUTO_SWAP_THRESHOLD`（与 Codex 一致），无需 Kimi 专属参数。
3. **先登再导入**：`subswap login kimi` 不复刻 OAuth，直接导入当前 `~/.kimi-code` 里已登录的凭证并置为 active；换下一个账号时用户先在 kimi 里登另一个，再 `subswap login kimi`。
4. **Codex 一起迁移到共享引擎**：抽象要用两个真实 provider 验证；Codex 专属逻辑（legacy 恢复、`chatgpt_account_id` 去重）作为 adapter 钩子保留，不丢。

## 3. 架构：文件型 OAuth 切换共享引擎

### 3.1 定位

现状 `CodexProvider` 把「切换机制」和「Codex 具体细节」揉在一个 ~900 行文件里；`ClaudeProvider` 因涉及钥匙串是另一套。Codex 与 Kimi 都是「一个 opaque JSON blob + 文件切换 + OAuth 刷新 + usage 查询」，机制完全同构。把机制抽出来，差异点用一个 adapter trait 表达。

### 3.2 位置

新增 crate `crates/providers/common`（`subswap-provider-common`），放引擎与 adapter trait。
`crates/providers/codex`、`crates/providers/kimi` 依赖它，各自只写 adapter。
（不放进 `core`：引擎带 reqwest/HTTP 依赖，core 保持纯数据/trait。）

### 3.3 引擎持有的「provider 无关」机制

`FileBlobProvider<A: FileBlobRuntime>`（持 `store`、`registry`、`home`）实现 `Provider` trait，并对外暴露与现有 `CodexProvider` 对等的公共方法：

- `activate`：`flock → 快照旧文件 → capture-on-leave 回灌 → 原子写新 blob → set_active`，任一步失败回滚（复用 `swap::swap_with_snapshot`）。
- `capture_live_into_store`：覆盖 live 前把 live 凭证回灌进其 owner 账号 store；**refresh-token 缺失守卫**——live 缺 refresh 且 store 有 refresh 时跳过，绝不写死账号（沿用 Codex 现有守卫与其排障结论）。
- `raw_blob_for_account`：active 账号读 live 文件（并顺手修复 store 副本），parked 账号读 store。
- `import_active` / `sync_active_metadata` / `import_from_file` / `import_raw_with_metadata`。
- `export_blob`（隔离物化）/ `absorb_blob`（隔离结束吸收轮换后的凭证）。
- `reconcile_active_from_live`（capture-on-arrival，只 live→store，补「绕过 swap 离开」的缺口）。
- `query_quota`：**parked 账号先按需刷新**（access token 过期/401 自愈，只刷 parked，从不刷 active/owner），再调 adapter 的 usage。死 token 守卫：`invalid_grant` 后不反复刷，标 `needs re-login`。
- `extract_access_token` / `extract_refresh_token`：递归 JSON 宽松查找，作为引擎默认实现（Codex/Kimi 通用）。

### 3.4 adapter trait（每个 runtime 的差异点）

```rust
#[async_trait]
pub trait FileBlobRuntime: Send + Sync + 'static {
    fn id() -> &'static str;                       // "codex" / "kimi"
    fn display_name() -> &'static str;
    fn home() -> PathBuf;                           // 读 env + 默认目录
    fn live_cred_path(home: &Path) -> PathBuf;      // auth.json / credentials/kimi-code.json

    /// 从 blob 抽最小元数据：primary_id、label、以及 usage/去重需要的字段。
    fn parse_metadata(blob: &str) -> BlobMetadata;

    /// 隔离运行：环境变量名 + 原生 CLI 名 + 可选的额外物化（如复制 config）。
    fn isolation() -> IsolationSpec;                // { env_var, native_cli, materialize_extra }

    /// 各自刷新端点；返回轮换后的完整 blob，或 DeadToken。
    async fn refresh(blob: &str) -> RefreshOutcome;

    /// 各自 usage 查询 → 归一化成 Vec<Quota>。
    async fn fetch_quota(access_token: &str, meta: &BlobMetadata) -> Result<Vec<Quota>>;

    // 可选钩子（默认空实现）：Codex 用来做 legacy 账号恢复 / chatgpt_account_id 去重。
    fn recover_legacy(_home: &Path, _account: &Account) -> Option<String> { None }
    fn dedup_key(_meta: &BlobMetadata) -> Option<String> { None }
}
```

差异点归属：

| 关注点 | Codex adapter | Kimi adapter |
|---|---|---|
| home / 文件 | `CODEX_HOME` / `auth.json` | `KIMI_CODE_HOME` / `credentials/kimi-code.json` |
| 元数据 | account_key/email/chatgpt_account_id（+ id_token JWT） | user_id/membership（access_token JWT） |
| refresh | 现有 Codex 刷新 | `POST auth.kimi.com/api/oauth/token`（form） |
| usage | `openai_usage` | `/usages`（本文 §1 的解析） |
| 隔离 | `CODEX_HOME` + 复制 config.toml + 原生 `codex` | `KIMI_CODE_HOME` + 原生 `kimi` |
| legacy 恢复/去重 | 保留为钩子 | 无（返回默认） |

### 3.5 引擎注册表驱动 CLI/daemon（去掉 match 分支）

现状 `run.rs` / `login.rs` 用 `match provider { "codex" => …, "claude" => … }` 硬编码。改为：
- 文件型 provider 统一实现一个对象安全的 `IsolatedRuntime` 接口（`export_blob`/`absorb_blob`/`materialize`/`isolation_env`/`native_cli`/`import_active`），由 `AppContext` 按 id 查表。
- `run` / `login` 的 Codex/Kimi 分支收敛成「查表 → 调统一接口」；Claude 因钥匙串+API 账号特殊，保留其专用分支。
- 新增文件型 runtime = 注册一个 adapter，**不改** `run.rs`/`login.rs` 的分支逻辑。

## 4. 具体改动清单

- `crates/providers/common/`（新）：引擎 + adapter trait + 通用 JSON 抽取 + 单测。
- `crates/providers/kimi/`（新）：`paths.rs`、`kimi_files.rs`（元数据）、`oauth.rs`（刷新）、`kimi_usage.rs`（usage 解析）、`lib.rs`（`KimiProvider = FileBlobProvider<KimiRuntime>` 组装 + adapter 实现）。
- `crates/providers/codex/`：改为基于共享引擎的薄 adapter；保留 legacy/dedup 钩子与其全部现有测试（回归基线）。
- `crates/cli/src/app.rs`：注册 `KimiProvider`；`AppContext` 增加文件型 runtime 查表。
- `crates/cli/src/cmd/login.rs`：加 `kimi` 分支（导入 active，不复刻 OAuth）。
- `crates/cli/src/cmd/run.rs`：`kimi` 走查表统一隔离流程（`KIMI_CODE_HOME`）。
- `crates/cli/src/cmd/default.rs`：默认入口对 Kimi `sync_active_metadata` 对齐 active。
- `crates/daemon/src/unix.rs`：注册 Kimi；把 `reconcile_active_from_live` 泛化到文件型 provider（capture-on-arrival）。Kimi 不做主动 keepalive（同 Codex，靠 query_quota 按需刷新）。
- `Cargo.toml`：新增两个成员 crate + workspace 依赖；升版本、同步 `Cargo.lock`。
- 文档：`AGENTS.md` 文档导航/目录速记/领域地图加 Kimi 与 common；`docs/PROVIDER_KNOWLEDGE_BASE.md` 增 Kimi 小节（端点、令牌、刷新、usage、不变量）；`docs/design/ARCHITECTURE.md` 增共享引擎分层；`docs/CLI.md` 增 `login/run kimi`。

## 5. 沿用的项目不变量

- 手动 `subswap swap` 永不依赖 quota；网络/quota/token 坏也能切走。
- active 账号只读不刷；refresh token 由原生客户端唯一轮换；引擎只刷 parked。
- `capture_live_into_store` 的 refresh 缺失守卫必须保留（防静默写死账号）。
- `async fn` 内阻塞 IO（文件锁、`std::fs`）包进 `spawn_blocking`。
- 写 `registry.toml` 的 `Option<T>` 加 `skip_serializing_if = "Option::is_none"`（避免 TOML null）。
- CLI/标识符统一 `swap`；`swap`/`rm` 编号走 `list_ordered()`。
- 跨模块阈值走 `settings::current()`，不在 provider/cli 硬编码。
- 不用高频 quota 请求模拟限流；查询前走 `quota_cache.json` 节流（默认 90s），daemon 与 CLI 共用。

## 6. 测试与验证

- **引擎单测**：activate 回滚、capture-on-leave 三态（live 有/无 refresh、两边都无）、raw_blob 优先级、reconcile。
- **Kimi 单测**：`kimi_files` 元数据解析、`kimi_usage` 解析（字符串数值、5h/7d 窗口映射、reset ISO8601）、refresh 死 token 守卫。用 `KIMI_CODE_OAUTH_HOST` / `KIMI_CODE_BASE_URL` 打到本地 mock，禁止连真实端点。
- **Codex 回归**：现有 codex 全部单测必须继续通过（迁移零回归的硬标准）。
- **隔离测试**：`cli_surface.rs` 的 `isolated_subswap` 增设 `KIMI_CODE_HOME` 重定向到一次性目录，禁止触碰真实 `~/.kimi-code`。
- **冒烟**：`subswap login kimi` 导入 → 默认入口显示 Kimi 行 + 5h/7d 额度 → `subswap swap` 切换 → `subswap run kimi` 隔离跑一次。
- `cargo test/build --workspace`、`cargo build --locked --release`，本机覆盖安装 + daemon 重启验证版本/哈希。

## 7. 非目标（YAGNI）

- 不做 `boosterWallet` 余额展示 / 充值提醒。
- 不复刻 Kimi 的 OAuth 设备码登录流程（先登再导入）。
- Kimi 不做 daemon 主动 keepalive（按需刷新足够）。
- 本期不迁移 Claude 到共享引擎（钥匙串语义不同，另议）。

## 8. 发布

按项目「改动即发布」：升 workspace 版本 + 同步 lock → 测试/构建/release 构建 → 覆盖安装 `subswap`/`subswapd` + 重启 daemon 验证 → 提交 + 打 tag + 推送 → 确认 GitHub Release（`update-homebrew.yml` 自动更新 formula）。

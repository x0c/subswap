# Kimi 接入 + 文件型 OAuth 切换共享引擎 设计

> 状态：**已落地**。权威行为与端点/不变量见 [`PROVIDER_KNOWLEDGE_BASE.md`](../../PROVIDER_KNOWLEDGE_BASE.md)（Kimi 节 + 共享引擎节）。本文保留产品决策、架构分工与非目标，供追溯。

## 1. 背景与调研结论（实测摘要）

Kimi Code CLI（`@moonshot-ai/kimi-code`）凭证形态与 Codex 同构：

| 项 | 事实 |
|---|---|
| 凭证文件 | `~/.kimi-code/credentials/kimi-code.json`；`KIMI_CODE_HOME` 可重定向 |
| 字段 | `access_token` / `refresh_token` / `expires_at` / `scope` / `token_type` / `expires_in` |
| 令牌 | access JWT，约 15 分钟；refresh 约 30 天、**单次轮换** |
| 身份 | JWT `user_id`（无 email）/`client_id`/`scope`；主键与默认 label = `user_id` |
| 钥匙串 | 不碰；纯文件切换 |
| 刷新 | `POST {KIMI_CODE_OAUTH_HOST:-https://auth.kimi.com}/api/oauth/token`，`form-urlencoded`：`client_id` + `grant_type=refresh_token` + `refresh_token`；`401/403/invalid_grant` → 死 token |
| 额度 | `GET {KIMI_CODE_BASE_URL:-https://api.kimi.com/coding/v1}/usages`；数值多为**字符串**；`usage` → 7d；`limits[]` 的 `duration+timeUnit` 换算分钟后 300→5h、10080→7d |
| YAGNI | `boosterWallet` 本期不进 quota |

解析/实现细节以 PROVIDER_KB 为准。

## 2. 已确认的产品决策

1. **完整对齐**：添加/导入、手动切换、默认入口、`subswap run kimi` 隔离、额度显示，与 Codex/Claude 同级。
2. **参与自动换号**：5h 窗口为判据，阈值走 `AUTO_SWAP_THRESHOLD`（同 Codex），无 Kimi 专属参数。
3. **先登再导入**：`subswap login kimi` 不复刻 OAuth，导入当前 `~/.kimi-code` 已登录凭证并置 active。
4. **Codex 迁入共享引擎**：用两个真实 provider 验证抽象；Codex legacy / `chatgpt_account_id` 去重以 adapter 钩子保留。

## 3. 架构：文件型 OAuth 切换共享引擎

### 3.1–3.2 定位与位置

机制（切换/回滚/回灌/隔离）与细节（解析/刷新/usage）分离。新增 `crates/providers/common`（`subswap-provider-common`）；`codex` / `kimi` 只写 adapter。不放 `core`（引擎带 HTTP）。Claude 因钥匙串 + API 账号形状不同，不迁入。

### 3.3 引擎机制（`FileBlobProvider<A: FileBlobRuntime>`）

| 机制 | 要点 |
|---|---|
| `activate` | flock → 快照 → capture-on-leave → 原子写 → `set_active`；失败回滚（`swap::swap_with_snapshot`） |
| `capture_live_into_store` | live→owner store；**缺 refresh 且 store 有 refresh → 跳过** |
| `raw_blob_for_account` | active 读 live（顺手修 store）；parked 读 store |
| 导入 | `import_active` / `sync_active_metadata` / `import_from_file` / `import_raw_with_metadata` |
| 隔离 | `export_blob` / `absorb_blob` |
| capture-on-arrival | `reconcile_active_from_live`（只 live→store） |
| `query_quota` | **只刷 parked**；死 token 后不反复刷，标 `needs re-login` |
| token 抽取 | `extract_access_token` / `extract_refresh_token` 递归宽松查找（引擎默认） |

### 3.4 adapter 差异点归属

| 关注点 | Codex | Kimi |
|---|---|---|
| home / 文件 | `CODEX_HOME` / `auth.json` | `KIMI_CODE_HOME` / `credentials/kimi-code.json` |
| 元数据 | `account_key`/`email`/`chatgpt_account_id`（+ id_token JWT） | `user_id`（access JWT） |
| refresh | 官方刷；adapter `Unsupported` | `POST …/api/oauth/token`（form） |
| usage | `openai_usage` | `/usages` |
| 隔离 | `CODEX_HOME` + 复制 `config.toml` + `codex` | `KIMI_CODE_HOME` + `kimi` |
| legacy/去重钩子 | 保留 | 默认空 |

必填：`id` / `display_name` / `home` / `live_cred_path` / `parse_metadata` / `isolation` / `refresh` / `fetch_quota`；可选 `recover_legacy` / `dedup_key` 等。完整方法表见 PROVIDER_KB 共享引擎节。

### 3.5 注册表驱动 CLI/daemon

文件型统一走对象安全隔离接口（`export_blob`/`absorb_blob`/`materialize`/`isolation_env`/`native_cli`/`import_active`），`AppContext` 按 id 查表。`run`/`login` 的 Codex/Kimi 收敛为查表；Claude 保留专用分支。新增文件型 = 注册 adapter，不改分支逻辑。

## 4. 落地范围（实现清单已完成）

已落地 crate/路径：`crates/providers/common/`、`crates/providers/kimi/`、Codex 薄 adapter、CLI/daemon 注册与查表、文档导航。细节以当前代码与 PROVIDER_KB 为准；本文不再维护逐文件改动清单。

## 5. 沿用的项目不变量

- 手动 `subswap swap` 永不依赖 quota。
- active 只读不刷；refresh 由原生唯一轮换；引擎只刷 parked。
- `capture_live_into_store` 的 refresh 缺失守卫必须保留。
- `async fn` 内阻塞 IO 包 `spawn_blocking`。
- 写 `registry.toml` 的 `Option<T>` 加 `skip_serializing_if = "Option::is_none"`。
- CLI/标识符统一 `swap`；`swap`/`rm` 编号走 `list_ordered()`。
- 跨模块阈值走 `settings::current()`，不在 provider/cli 硬编码。
- 不用高频 quota 模拟限流；查询前 `quota_cache.json` 节流（默认 90s），daemon 与 CLI 共用。

## 6. 测试与验证（设计期口径）

- 引擎：activate 回滚、capture-on-leave 三态、raw_blob 优先级、reconcile
- Kimi：元数据、usage（字符串数值、5h/7d、ISO8601 reset）、refresh 死 token；mock 用 `KIMI_CODE_OAUTH_HOST` / `KIMI_CODE_BASE_URL`，禁连真实端点
- Codex 现有单测零回归；隔离测试 `KIMI_CODE_HOME` 重定向，禁碰真实 `~/.kimi-code`
- 冒烟：`login kimi` → 默认入口 5h/7d → `swap` → `run kimi`
- `cargo test/build --workspace`、`cargo build --locked --release`；覆盖安装 + daemon 重启验版本

## 7. 非目标（YAGNI）

- 不做 `boosterWallet` 展示/充值提醒
- 不复刻 OAuth 设备码登录
- Kimi 不做 daemon 主动 keepalive
- 本期不迁 Claude 到共享引擎

## 8. 发布

按「改动即发布」：升版本 → 测/构建 → 覆盖安装 `subswap`/`subswapd` + 重启 daemon → 提交/tag/推送 → 确认 GitHub Release（`update-homebrew.yml` 更新 formula）。

<!-- 该文档整理/压缩于 2026-09-05 -->

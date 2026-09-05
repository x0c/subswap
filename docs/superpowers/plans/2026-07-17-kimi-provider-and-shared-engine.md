# Kimi Provider 与文件型 OAuth 切换共享引擎 — 历史实现计划

> **状态（2026-07-17）：已落地。** 日常无需翻阅。权威现状：`docs/PROVIDER_KNOWLEDGE_BASE.md`、架构文档、源码。
> 配对设计稿：`docs/superpowers/specs/2026-07-17-kimi-provider-and-shared-engine-design.md`。
> 本篇仅保留仍影响后续判断的不变量与架构裁定；已完成 checkbox / 大段实现代码 / 提交流水已删。

**Goal（已完成）：** 加 Kimi（Moonshot）Provider；把 Codex/Kimi 共有的「文件型 OAuth 账号切换」抽成共享引擎；新 file-blob runtime 只写薄适配器。

**Architecture（已落地）：** `crates/providers/common`：`FileBlobRuntime` adapter trait + `FileBlobProvider<A>` 引擎（flock+快照+回滚、capture-on-leave/arrival、parked 按需刷新、隔离 export/absorb）。Codex / Kimi（及后续 OpenCode）各一个 adapter；CLI `run`/`login` 注册表驱动。Claude 因钥匙串暂独立，不进本引擎。

**Tech：** Rust 2021 workspace；tokio + `spawn_blocking`；reqwest(rustls)；凭证明文 `FileStore`（不碰 macOS 钥匙串）。

---

## 仍有效的不变量（Global Constraints）

- 手动 `subswap swap` 永不依赖 quota：网络/quota/token 坏也要能切走。
- active 账号只读不刷；refresh token 由原生客户端唯一轮换；引擎只刷 parked 账号。
- `capture_live_into_store` 守卫：live 缺 refresh 且 store 有 refresh 时跳过覆盖（防静默写死账号）。
- `async fn` 内阻塞 IO（文件锁、`std::fs`、HTTP 阻塞）必须 `tokio::task::spawn_blocking`；**禁止**对 activate/capture 用 `block_in_place`（current-thread 测试 runtime 会 panic）。
- 写入 `registry.toml` 的 `Option<T>` 须 `#[serde(skip_serializing_if = "Option::is_none")]`（TOML 不支持 null）。
- CLI / Rust 标识符 / 英文文案统一用 `swap`，不用 `switch`。
- 跨模块阈值走 `subswap_core::settings::current()`；自动切换阈值只认 `defaults::AUTO_SWAP_THRESHOLD`。
- 代码注释 / doc comment 中文；用户可见输出、错误、tracing 英文且简洁。
- quota 查询前走 `quota_cache.json` 节流（默认 90s），daemon 与 CLI 共用。
- 集成测试禁止触碰真实 `~/.kimi-code`：路径经 `KIMI_CODE_HOME`；HTTP 经 `KIMI_CODE_OAUTH_HOST` / `KIMI_CODE_BASE_URL` 打 mock。

Kimi 凭证路径、刷新/usage 端点、15min access、5h/7d 窗口映射等**实测常量**以 `PROVIDER_KNOWLEDGE_BASE`「Kimi / Moonshot」节为准，本计划不重复维护。

---

## 关键类型与模块（仍对应源码）

| 符号 / 路径 | 作用 |
|---|---|
| `crates/providers/common`（`subswap-provider-common`） | 共享引擎 crate |
| `json::{extract_token, extract_access_token, extract_refresh_token}` | 嵌套 JSON 递归抽非空字符串字段 |
| `BlobMetadata { primary_id, label, dedup_key, extra }` | blob → registry 元数据 |
| `IsolationSpec { env_var, native_cli }` | 隔离 env + 原生 CLI 名 |
| `RefreshOutcome::{Rotated, DeadToken, Unsupported}` | parked 刷新结果 |
| `FileBlobRuntime` | adapter 差异点契约（见下） |
| `FileBlobProvider<A>`（`engine.rs`） | 机制：import / activate / capture / reconcile / export / absorb / `query_quota` |
| `IsolatedProvider`（`isolated.rs`） | `run` 隔离目录物化 |
| `crates/providers/kimi`：`paths` / `kimi_files` / `oauth` / `kimi_usage` / `lib` | Kimi adapter；`type KimiProvider = FileBlobProvider<KimiRuntime>` |
| Codex `store_field() → "auth_json"` | 兼容迁移前 store 字段，免数据迁移 |
| Codex `dedup_extra_key() → "chatgpt_account_id"` | 兼容旧 `registry.toml` 去重键名（默认键为 `"dedup_key"`） |

`FileBlobRuntime` 核心方法（实现后源码还扩展了 `extract_blob` / `compose_live` / `access_token` / `isolation_rel_path` / `isolation_extra_env` 等，以 `runtime.rs` 为准）：

- `id` / `display_name` / `store_field`（默认 `"blob"`）/ `dedup_extra_key`（默认 `"dedup_key"`）
- `home` / `live_cred_path` / `parse_metadata` / `isolation`
- `refresh`（仅 parked）/ `fetch_quota`
- 可选：`recover_legacy` / `materialize_extra`

引擎公开面（摘要）：`new` / `import_*` / `export_blob` / `absorb_blob` / `raw_blob_for_account` / `reconcile_active_from_live` / `isolation` / `home` + `impl Provider`。

---

## 交付范围（历史实施记录，细节见 spec / PROVIDER_KB）

| Phase | 内容 | 验收硬线（当时） |
|---|---|---|
| 1 | common：json + trait + `FileBlobProvider` | `cargo test -p subswap-provider-common` |
| 2 | Kimi crate：路径/JWT 元数据、OAuth 刷新、usage 5h+7d、组装 `KimiProvider` | 不触真实 `~/.kimi-code`；mock 端点 |
| 3 | Codex 迁入引擎（adapter + `auth_json` / legacy / dedup 钩子） | **现有 Codex 单测全绿、行为零回归** |
| 4 | app/daemon 注册 Kimi；`login kimi`（先登再导入）；`run` 注册表驱动隔离 | 注册表查表，无 provider 硬编码分支 |
| 5 | 文档同步 + 版本发布 | AGENTS / PROVIDER_KB / ARCHITECTURE / CLI |

**自动换号：** Kimi 参与自动换号无需改 `auto_policy`——按 provider 无差别读各账号 5h 窗口与 `AUTO_SWAP_THRESHOLD`；daemon 注册且 `query_quota` 返回 5h 即可。若实现时发现有 provider 白名单，再补 `"kimi"`。

**Spec 覆盖：** 设计稿 §1–§8 ↔ 上表 Phase 1–5；不变量见 Global Constraints + 引擎 capture/parked/`spawn_blocking`。

---

## 接新 file-blob provider 时仍适用的裁定

1. 只实现 `FileBlobRuntime`（+ 必要时覆盖 `store_field` / `dedup_extra_key` / `extract_blob` / `compose_live`），机制不重写。
2. Codex 迁移教训：私有解析/legacy/usage 留在 provider crate；机制进 common；以现有单测零回归为硬线。
3. Claude 钥匙串路径不塞进本引擎。

<!-- 该文档整理/压缩于 2026-09-05 -->

# Kimi Provider 与文件型 OAuth 切换共享引擎 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 subswap 加 Kimi（Moonshot）Provider，并把 Codex 与 Kimi 共有的「文件型 OAuth 账号切换」机制抽成共享引擎，未来接新 agent runtime 只写一个薄适配器。

**Architecture:** 新增 `crates/providers/common`，内含 `FileBlobRuntime` adapter trait 与 `FileBlobProvider<A>` 引擎（持有 flock+快照+回滚、capture-on-leave/arrival、parked 按需刷新、隔离 export/absorb 等 provider 无关机制）。Codex 与 Kimi 各写一个 adapter；`run.rs`/`login.rs` 改为注册表驱动，去掉 provider 硬编码分支。

**Tech Stack:** Rust 2021（workspace，rust-version 1.80），tokio + `spawn_blocking`，reqwest(rustls)，serde/serde_json，async-trait，chrono。凭证明文 `FileStore`（不碰 macOS 钥匙串）。

## Global Constraints

- 手动 `subswap swap` 永不依赖 quota：网络/quota/token 坏也要能切走。
- active 账号只读不刷；refresh token 由原生客户端唯一轮换；引擎只刷 parked 账号。
- `capture_live_into_store` 的 refresh 缺失守卫必须保留：live 缺 refresh 且 store 有 refresh 时跳过覆盖（防静默写死账号）。
- `async fn` 内阻塞 IO（文件锁、`std::fs`、HTTP 阻塞调用）必须包进 `tokio::task::spawn_blocking`。
- 写入 `registry.toml` 的 `Option<T>` 字段必须加 `#[serde(skip_serializing_if = "Option::is_none")]`（TOML 不支持 null）。
- CLI 子命令、Rust 标识符、英文文案统一用 `swap`，不用 `switch`。
- 跨模块阈值/窗口/百分比走 `subswap_core::settings::current()`，不在 provider/cli 硬编码；自动切换阈值只认 `defaults::AUTO_SWAP_THRESHOLD`。
- 代码注释、doc comment 用中文；用户可见输出、错误文本、tracing message 用英文且简洁。
- quota 查询前走 `quota_cache.json` 节流（默认 90s），daemon 与 CLI 共用；禁止用高频请求模拟限流。
- 集成测试禁止触碰真实 `~/.kimi-code`：所有路径经 `KIMI_CODE_HOME` 重定向到一次性目录；HTTP 经 `KIMI_CODE_OAUTH_HOST`/`KIMI_CODE_BASE_URL` 打到 mock。
- 版本：升 workspace 版本并同步 `Cargo.lock`；发布走项目「改动即发布」流程。

## 实测常量（供各任务直接引用）

- Kimi 凭证：`$KIMI_CODE_HOME`（默认 `~/.kimi-code`）`/credentials/kimi-code.json`，字段 `access_token`/`refresh_token`/`expires_at`/`scope`/`token_type`/`expires_in`。
- access_token 是 JWT，payload 含 `user_id`/`client_id`/`scope`/`exp`；15 分钟过期。
- 刷新：`POST {KIMI_CODE_OAUTH_HOST|https://auth.kimi.com}/api/oauth/token`，
  `Content-Type: application/x-www-form-urlencoded`，`Accept: application/json`，
  body `client_id=<id>&grant_type=refresh_token&refresh_token=<token>`；
  `401/403` 或返回 `error=invalid_grant` → 死 token。
- usage：`GET {KIMI_CODE_BASE_URL|https://api.kimi.com/coding/v1}/usages`，`Authorization: Bearer <access>`，`User-Agent: subswap`。
- usage 响应（数值为**字符串**，reset 为 ISO8601）：
  - `usage`：`{limit,used,remaining,resetTime}` → 7d 窗口。
  - `limits[].window`：`{duration,timeUnit}`（`300 TIME_UNIT_MINUTE` = 5h）；`limits[].detail`：`{limit,used,remaining,resetTime}`。

---

## Phase 1 — 共享引擎 crate `subswap-provider-common`

### Task 1: 新建 crate 脚手架 + 通用 JSON token 抽取

**Files:**
- Create: `crates/providers/common/Cargo.toml`
- Create: `crates/providers/common/src/lib.rs`
- Create: `crates/providers/common/src/json.rs`
- Modify: `Cargo.toml`（workspace `members` + `workspace.dependencies`）

**Interfaces:**
- Produces:
  - `pub fn extract_token(raw: &str, key: &str) -> Option<String>` — 递归查找非空字符串字段。
  - `pub fn extract_access_token(raw: &str) -> Option<String>`（= `extract_token(raw, "access_token")`）。
  - `pub fn extract_refresh_token(raw: &str) -> Option<String>`（= `extract_token(raw, "refresh_token")`，仅返回非空）。

- [ ] **Step 1: 加 workspace 成员与依赖**

在根 `Cargo.toml` 的 `[workspace] members` 增加两行（Kimi 在 Phase 3 建，但一起登记避免二次改动）：

```toml
members = [
    "crates/core",
    "crates/cli",
    "crates/daemon",
    "crates/providers/common",
    "crates/providers/codex",
    "crates/providers/claude",
    "crates/providers/kimi",
]
```

在 `[workspace.dependencies]` 末尾（`subswap-daemon` 那几行附近）追加：

```toml
subswap-provider-common = { path = "crates/providers/common" }
subswap-provider-kimi = { path = "crates/providers/kimi" }
```

- [ ] **Step 2: 写 crate 清单**

`crates/providers/common/Cargo.toml`：

```toml
[package]
name = "subswap-provider-common"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
subswap-core = { workspace = true }
anyhow = { workspace = true }
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
reqwest = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
chrono = { workspace = true }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 3: 写失败测试** — `crates/providers/common/src/json.rs`

```rust
//! 半结构化凭证 JSON 的宽松字段抽取。Codex/Kimi 的 blob 结构各异，只按 key 递归找。

/// 在任意嵌套 JSON 里递归查找第一个名为 `key` 的非空字符串值。
pub fn extract_token(raw: &str, key: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    fn walk(v: &serde_json::Value, key: &str) -> Option<String> {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(s)) = map.get(key) {
                    if !s.is_empty() {
                        return Some(s.clone());
                    }
                }
                map.values().find_map(|c| walk(c, key))
            }
            serde_json::Value::Array(items) => items.iter().find_map(|c| walk(c, key)),
            _ => None,
        }
    }
    walk(&value, key)
}

/// 抽 access_token（兼容扁平与 `tokens.access_token` 嵌套）。
pub fn extract_access_token(raw: &str) -> Option<String> {
    extract_token(raw, "access_token")
}

/// 抽非空 refresh_token。
pub fn extract_refresh_token(raw: &str) -> Option<String> {
    extract_token(raw, "refresh_token")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_and_flat() {
        assert_eq!(
            extract_access_token(r#"{"tokens":{"access_token":"t1"}}"#).as_deref(),
            Some("t1")
        );
        assert_eq!(
            extract_access_token(r#"{"access_token":"t2"}"#).as_deref(),
            Some("t2")
        );
    }

    #[test]
    fn empty_refresh_is_none() {
        assert!(extract_refresh_token(r#"{"refresh_token":""}"#).is_none());
        assert!(extract_refresh_token(r#"{"access_token":"x"}"#).is_none());
    }
}
```

- [ ] **Step 4: 写 lib.rs 导出**

`crates/providers/common/src/lib.rs`：

```rust
//! 文件型 OAuth 账号切换共享引擎：Codex / Kimi 等「一个 JSON blob + 文件切换 + OAuth 刷新 + usage」
//! 形态的 provider 共用的机制。差异点由 [`FileBlobRuntime`] adapter 表达。

pub mod json;

pub use json::{extract_access_token, extract_refresh_token, extract_token};
```

- [ ] **Step 5: 运行测试**

Run: `cargo test -p subswap-provider-common`
Expected: PASS（2 个测试）。

- [ ] **Step 6: 提交**

```bash
git add Cargo.toml crates/providers/common
git commit -m "feat(common): scaffold shared provider engine crate with JSON token extraction"
```

### Task 2: 定义 adapter trait 与共享类型

**Files:**
- Create: `crates/providers/common/src/runtime.rs`
- Modify: `crates/providers/common/src/lib.rs`

**Interfaces:**
- Produces:
  - `pub struct BlobMetadata { pub primary_id: Option<String>, pub label: Option<String>, pub dedup_key: Option<String>, pub extra: serde_json::Map<String, serde_json::Value> }`
  - `pub struct IsolationSpec { pub env_var: &'static str, pub native_cli: &'static str }`
  - `pub enum RefreshOutcome { Rotated(String), DeadToken, Unsupported }`
  - `#[async_trait] pub trait FileBlobRuntime: Send + Sync + 'static`（方法见下）。
- Consumes: `subswap_core::{Account, Quota}`。

- [ ] **Step 1: 写 runtime.rs**

```rust
//! 文件型 provider 的 adapter 契约：每个 runtime 只实现差异点，机制在 [`crate::engine`]。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::{Account, Quota};

/// 从凭证 blob 解析出的最小元数据。所有可选字段最终可能进 registry.toml，
/// 注意：写进 `extra` 的 `Option` 值不要保留 null（引擎只写 `Some`）。
#[derive(Debug, Clone, Default)]
pub struct BlobMetadata {
    /// account 主键候选（如 Codex account_key / Kimi user_id）。
    pub primary_id: Option<String>,
    /// 展示 label（如 email / user_id）。
    pub label: Option<String>,
    /// 跨主键去重用的稳定键（如 Codex chatgpt_account_id）；无则 None。
    pub dedup_key: Option<String>,
    /// 额外落进 registry.toml `extra` 的字段（provider 私有，如会员档/额度用 header）。
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 隔离运行所需的差异点。
#[derive(Debug, Clone)]
pub struct IsolationSpec {
    /// 隔离环境变量名（Codex `CODEX_HOME` / Kimi `KIMI_CODE_HOME`）。
    pub env_var: &'static str,
    /// 原生 CLI 可执行名（`codex` / `kimi`）。
    pub native_cli: &'static str,
}

/// 刷新结果。
pub enum RefreshOutcome {
    /// 刷新成功，返回轮换后的完整 blob。
    Rotated(String),
    /// refresh token 已失效（invalid_grant / 401 / 403），需重新登录。
    DeadToken,
    /// 该 runtime 不支持刷新（如纯 API key）。
    Unsupported,
}

/// 每个文件型 runtime 的差异点契约。机制（切换/回滚/回灌/隔离）在引擎里，不在这里。
#[async_trait]
pub trait FileBlobRuntime: Send + Sync + 'static {
    /// provider 标识，如 "codex" / "kimi"。
    fn id(&self) -> &'static str;
    /// 人类可读名称。
    fn display_name(&self) -> &'static str;
    /// 解析 provider 工作目录（读 env + 默认目录）。
    fn home(&self) -> PathBuf;
    /// 工作目录内的 live 凭证文件路径。
    fn live_cred_path(&self, home: &Path) -> PathBuf;
    /// 从 blob 抽最小元数据。解析失败返回 `Default`（透传策略，不 panic）。
    fn parse_metadata(&self, blob: &str) -> BlobMetadata;
    /// 隔离运行差异点。
    fn isolation(&self) -> IsolationSpec;

    /// 刷新一个 blob，返回轮换后的完整 blob。仅对 parked 账号调用。
    async fn refresh(&self, blob: &str) -> Result<RefreshOutcome>;
    /// 查询额度。`access_token` 由引擎保证是新鲜的（parked 已按需刷新）。
    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>>;

    /// 可选：在 store/live 都拿不到时，从 provider 私有 legacy 布局恢复 blob。默认无。
    fn recover_legacy(&self, _home: &Path, _account: &Account) -> Option<String> {
        None
    }

    /// 可选：额外物化（如复制真实 config 进隔离目录）。默认无。
    fn materialize_extra(&self, _home: &Path, _env_dir: &Path) {}
}
```

- [ ] **Step 2: 导出**

在 `crates/providers/common/src/lib.rs` 增加：

```rust
pub mod runtime;

pub use runtime::{BlobMetadata, FileBlobRuntime, IsolationSpec, RefreshOutcome};
```

- [ ] **Step 3: 编译**

Run: `cargo check -p subswap-provider-common`
Expected: 通过（trait 暂无实现，允许无测试）。

- [ ] **Step 4: 提交**

```bash
git add crates/providers/common/src
git commit -m "feat(common): define FileBlobRuntime adapter trait and shared types"
```

### Task 3: 引擎 `FileBlobProvider<A>` — 存取、导入、切换、回灌

**Files:**
- Create: `crates/providers/common/src/engine.rs`
- Modify: `crates/providers/common/src/lib.rs`

**Interfaces:**
- Produces（`impl<A: FileBlobRuntime> FileBlobProvider<A>` 及 `impl Provider`）：
  - `pub fn new(runtime: A, store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> Self`
  - `pub fn import_active(&self, label_hint: Option<String>) -> Result<Account>`
  - `pub fn sync_active_metadata(&self, label_hint: Option<String>) -> Result<Account>`
  - `pub fn import_from_file(&self, path: PathBuf, label_hint: Option<String>) -> Result<Account>`
  - `pub fn import_raw(&self, raw: String, label_hint: Option<String>, active: Option<bool>) -> Result<Account>`
  - `pub fn export_blob(&self, id: &AccountId) -> Result<String>`
  - `pub fn absorb_blob(&self, id: &AccountId, raw: &str) -> Result<()>`
  - `pub fn raw_blob_for_account(&self, account: &Account) -> Result<String>`（供 quota 与 export 复用）
  - `pub fn reconcile_active_from_live(&self) -> Result<()>`
  - `pub fn isolation(&self) -> IsolationSpec` / `pub fn home(&self) -> PathBuf`
  - `impl Provider`：`id`/`display_name`/`client_targets`/`list_accounts`/`activate`/`query_quota`
- Consumes: Task 2 的 trait，`subswap_core::swap::{swap_with_snapshot, SwapTarget}`，`common::json`。

> 说明：本引擎是把现有 `crates/providers/codex/src/lib.rs` 的 provider 无关部分泛化而来。
> `AUTH_FIELD` 统一命名为 `"blob"`（Codex 迁移任务会做旧字段 `"auth_json"` 的兼容读取，见 Task 6）。
> 常量 `const BLOB_FIELD: &str = "blob";`，`const META_DEDUP: &str = "dedup_key";`。

- [ ] **Step 1: 写引擎骨架（结构 + 原子写 + 存账号）**

`crates/providers/common/src/engine.rs`：

```rust
//! 文件型 provider 的 provider 无关机制。差异点见 [`crate::FileBlobRuntime`]。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use subswap_core::error::{Error, Result};
use subswap_core::swap::{swap_with_snapshot, SwapTarget};
use subswap_core::{
    Account, AccountId, AccountRegistry, ClientTarget, CredentialStore, Provider, Quota,
};

use crate::json::{extract_access_token, extract_refresh_token};
use crate::runtime::{BlobMetadata, FileBlobRuntime, IsolationSpec, RefreshOutcome};

/// keyring/FileStore 字段名。整段 blob 存这里。
const BLOB_FIELD: &str = "blob";
/// registry.toml `extra` 里存去重键。
const META_DEDUP: &str = "dedup_key";

pub struct FileBlobProvider<A: FileBlobRuntime> {
    runtime: A,
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    home: PathBuf,
}

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    pub fn new(
        runtime: A,
        store: Arc<dyn CredentialStore>,
        registry: Arc<AccountRegistry>,
    ) -> Self {
        let home = runtime.home();
        Self { runtime, store, registry, home }
    }

    pub fn home(&self) -> PathBuf {
        self.home.clone()
    }

    pub fn isolation(&self) -> IsolationSpec {
        self.runtime.isolation()
    }

    fn live_path(&self) -> PathBuf {
        self.runtime.live_cred_path(&self.home)
    }

    /// 原子写 live 凭证：tmp + rename + 0o600。
    fn write_blob(path: &Path, contents: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }
}
```

- [ ] **Step 2: 存账号 + 导入方法**

在同一 `impl` 块内追加：

```rust
impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 从 blob 落库：blob 进 store，元数据进 registry.toml。`active_override=None` 时保留原 active。
    fn store_account(
        &self,
        raw: String,
        label_hint: Option<String>,
        active_override: Option<bool>,
    ) -> Result<Account> {
        let meta = self.runtime.parse_metadata(&raw);
        let id_string = meta
            .primary_id
            .clone()
            .or_else(|| label_hint.clone())
            .ok_or_else(|| {
                Error::Provider(
                    "cannot parse account id from credentials; pass --label to set it explicitly"
                        .into(),
                )
            })?;
        let id = AccountId(id_string);
        let label = label_hint
            .or_else(|| meta.label.clone())
            .unwrap_or_else(|| id.0.clone());

        self.store.set(self.runtime.id(), id.0.as_str(), BLOB_FIELD, &raw)?;

        let mut extra = meta.extra.clone();
        if let Some(dk) = meta.dedup_key.clone() {
            extra.insert(META_DEDUP.into(), serde_json::Value::String(dk));
        }

        let existing = self
            .registry
            .find(self.runtime.id(), &id)?
            .or_else(|| self.find_by_dedup(&meta));
        if let Some(ex) = existing.as_ref() {
            if ex.id != id {
                let _ = self.store.delete(self.runtime.id(), ex.id.0.as_str(), BLOB_FIELD);
                self.registry.remove(self.runtime.id(), &ex.id)?;
            }
        }

        let account = Account {
            provider: self.runtime.id().into(),
            id: id.clone(),
            label,
            active: active_override.unwrap_or_else(|| existing.as_ref().is_some_and(|a| a.active)),
            created_at: existing.as_ref().map(|a| a.created_at).unwrap_or_else(Utc::now),
            last_used_at: existing.and_then(|a| a.last_used_at),
            priority: 100,
            extra,
        };
        self.registry.upsert(account.clone())?;
        Ok(account)
    }

    fn find_by_dedup(&self, meta: &BlobMetadata) -> Option<Account> {
        let target = meta.dedup_key.as_deref()?;
        self.registry
            .list_by_provider(self.runtime.id())
            .ok()?
            .into_iter()
            .find(|a| a.extra.get(META_DEDUP).and_then(|v| v.as_str()) == Some(target))
    }

    /// 从当前 live 文件导入（可切换）。
    pub fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
        let raw = fs::read_to_string(self.live_path())
            .map_err(|e| Error::Provider(format!("read live credentials failed: {e}")))?;
        self.store_account(raw, label_hint, Some(true))
    }

    /// 只对齐当前 live 的元数据 active 标记，不写凭证（默认入口用，避免弹钥匙串）。
    pub fn sync_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        let raw = fs::read_to_string(self.live_path())
            .map_err(|e| Error::Provider(format!("read live credentials failed: {e}")))?;
        // 复用 store_account，但不覆盖 store 里已有的可切换 blob：仅当 store 尚无该账号时才写。
        let meta = self.runtime.parse_metadata(&raw);
        let has_blob = meta
            .primary_id
            .as_ref()
            .map(|pid| {
                self.store
                    .get(self.runtime.id(), pid, BLOB_FIELD)
                    .ok()
                    .flatten()
                    .is_some()
            })
            .unwrap_or(false);
        if has_blob {
            // 已有可切换副本：只更新 active 标记，不重写 blob。
            let pid = meta.primary_id.clone().unwrap();
            let id = AccountId(pid);
            self.registry.set_active(self.runtime.id(), &id)?;
            return self
                .registry
                .find(self.runtime.id(), &id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: self.runtime.id().into(),
                    id: id.to_string(),
                });
        }
        self.store_account(raw, label_hint, Some(true))
    }

    /// 从任意文件导入（校验合法 JSON）。
    pub fn import_from_file(&self, path: PathBuf, label_hint: Option<String>) -> Result<Account> {
        let raw = fs::read_to_string(&path)?;
        serde_json::from_str::<serde_json::Value>(&raw)
            .map_err(|e| Error::Provider(format!("{} is not valid JSON: {e}", path.display())))?;
        self.store_account(raw, label_hint, None)
    }

    /// 从原始 blob 导入（migrate 用）。
    pub fn import_raw(
        &self,
        raw: String,
        label_hint: Option<String>,
        active: Option<bool>,
    ) -> Result<Account> {
        self.store_account(raw, label_hint, active)
    }
}
```

- [ ] **Step 3: 编译占位**

Run: `cargo check -p subswap-provider-common`
Expected: 报错「缺少 activate/query_quota 等」尚可——继续 Step 4 补齐后再编译。（若想分步，可临时 `#[allow(dead_code)]`。）

- [ ] **Step 4: raw_blob / capture 守卫 / reconcile / export / absorb**

追加 `impl` 块：

```rust
impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 取账号 blob：active 账号优先读 live（并顺手修复 store 副本），parked 读 store，最后 legacy。
    pub fn raw_blob_for_account(&self, account: &Account) -> Result<String> {
        if let Some(raw) = self.read_live_if_matches(account) {
            let _ = self
                .store
                .set(self.runtime.id(), account.id.0.as_str(), BLOB_FIELD, &raw);
            return Ok(raw);
        }
        if let Some(raw) = self.store.get(self.runtime.id(), account.id.0.as_str(), BLOB_FIELD)? {
            return Ok(raw);
        }
        if let Some(raw) = self.runtime.recover_legacy(&self.home, account) {
            let _ = self
                .store
                .set(self.runtime.id(), account.id.0.as_str(), BLOB_FIELD, &raw);
            return Ok(raw);
        }
        Err(Error::Credential(format!(
            "no stored credentials for {}:{}; run `subswap login {}` or re-import",
            self.runtime.id(),
            account.id,
            self.runtime.id()
        )))
    }

    fn read_live_if_matches(&self, account: &Account) -> Option<String> {
        let raw = fs::read_to_string(self.live_path()).ok()?;
        let meta = self.runtime.parse_metadata(&raw);
        self.metadata_matches(&meta, account).then_some(raw)
    }

    fn metadata_matches(&self, meta: &BlobMetadata, account: &Account) -> bool {
        let id_hit = meta.primary_id.as_deref() == Some(account.id.0.as_str())
            || meta.label.as_deref() == Some(account.id.0.as_str());
        let dedup_hit = match (meta.dedup_key.as_deref(), account.extra.get(META_DEDUP)) {
            (Some(dk), Some(v)) => v.as_str() == Some(dk),
            _ => false,
        };
        id_hit || dedup_hit
    }

    /// 覆盖 live 前把 live 凭证回灌进其 owner 账号 store。
    /// 守卫：live 缺 refresh 且 store 已有 refresh 时跳过（防静默写死账号）。
    fn capture_live_into_store(&self) -> Result<()> {
        let live_raw = match fs::read_to_string(self.live_path()) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        let meta = self.runtime.parse_metadata(&live_raw);
        let owner = self
            .registry
            .list_by_provider(self.runtime.id())?
            .into_iter()
            .find(|a| self.metadata_matches(&meta, a));
        let Some(owner) = owner else { return Ok(()) };

        if extract_refresh_token(&live_raw).is_none() {
            if let Some(existing) = self.store.get(self.runtime.id(), owner.id.0.as_str(), BLOB_FIELD)? {
                if extract_refresh_token(&existing).is_some() {
                    tracing::warn!(account = %owner.id, "live capture missing refresh_token; skipped overwrite");
                    return Ok(());
                }
            }
        }
        self.store.set(self.runtime.id(), owner.id.0.as_str(), BLOB_FIELD, &live_raw)?;
        Ok(())
    }

    /// daemon capture-on-arrival：只 live→store，不碰 active 标记。
    pub fn reconcile_active_from_live(&self) -> Result<()> {
        self.capture_live_into_store()
    }

    /// 隔离物化用：导出账号 blob。
    pub fn export_blob(&self, id: &AccountId) -> Result<String> {
        let account = self.require_account(id)?;
        self.raw_blob_for_account(&account)
    }

    /// 隔离结束吸收（可能轮换过的）凭证，仅更新 store 副本。
    pub fn absorb_blob(&self, id: &AccountId, raw: &str) -> Result<()> {
        serde_json::from_str::<serde_json::Value>(raw)
            .map_err(|e| Error::Provider(format!("isolated credentials not valid JSON: {e}")))?;
        self.store.set(self.runtime.id(), id.0.as_str(), BLOB_FIELD, raw)?;
        Ok(())
    }

    fn require_account(&self, id: &AccountId) -> Result<Account> {
        self.registry
            .find(self.runtime.id(), id)?
            .ok_or_else(|| Error::AccountNotFound {
                provider: self.runtime.id().into(),
                id: id.to_string(),
            })
    }
}
```

- [ ] **Step 5: `impl Provider`（activate + query_quota，含 parked 按需刷新）**

```rust
#[async_trait]
impl<A: FileBlobRuntime> Provider for FileBlobProvider<A> {
    fn id(&self) -> &'static str {
        self.runtime.id()
    }
    fn display_name(&self) -> &'static str {
        self.runtime.display_name()
    }
    fn client_targets(&self) -> Vec<ClientTarget> {
        vec![ClientTarget {
            id: format!("{}_live", self.runtime.id()),
            display_name: format!("{} credentials", self.runtime.display_name()),
            probe_path: self.live_path(),
        }]
    }
    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(self.runtime.id())
    }

    async fn activate(&self, id: &AccountId) -> Result<()> {
        let account = self.require_account(id)?;
        let target_raw = self.raw_blob_for_account(&account)?;

        // 把需要移动进 spawn_blocking 的东西克隆出来。
        let home = self.home.clone();
        let live_path = self.live_path();
        let provider_id = self.runtime.id();
        let registry = self.registry.clone();
        let id_owned = id.clone();

        // capture-on-leave 需要 &self 的能力：先在异步侧同步执行（纯本地 IO，量小）。
        // 注意：仍在 async 上下文，这里的 IO 很轻（读一次 live + 写一次 store）。
        // 若要严格无阻塞，可包 spawn_blocking；此处与 Codex 现有实现保持一致的取舍。
        tokio::task::block_in_place(|| self.capture_live_into_store())
            .unwrap_or_else(|e| tracing::warn!(err = %e, "capture-on-leave failed; continuing swap"));

        tokio::task::spawn_blocking(move || {
            let blob = target_raw;
            let targets = vec![SwapTarget {
                snapshot_name: "credentials",
                live_path,
                writer: Box::new(move |p| FileBlobProvider::<A>::write_blob(p, &blob)),
            }];
            swap_with_snapshot(provider_id, &home, targets, || {
                registry.set_active(provider_id, &id_owned)
            })
        })
        .await
        .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))?
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        let account = self.require_account(id)?;
        let mut raw = self.raw_blob_for_account(&account)?;

        // parked 账号：access token 大概率过期，先按需刷新（active 账号不刷）。
        if !account.active {
            if let Some(fresh) = self.refresh_parked_if_needed(&account, &raw).await? {
                raw = fresh;
            }
        }
        let access = extract_access_token(&raw).ok_or_else(|| {
            Error::QuotaFetch("no access_token in credentials; schema may have changed".into())
        })?;
        self.runtime.fetch_quota(&access, &account).await
    }
}

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// parked 账号刷新一次并写回 store。返回 Some(新 blob) 表示已轮换。
    /// DeadToken → 标 needs re-login（写 registry extra），返回原样让上层照旧尝试（会失败并提示）。
    async fn refresh_parked_if_needed(&self, account: &Account, raw: &str) -> Result<Option<String>> {
        match self.runtime.refresh(raw).await? {
            RefreshOutcome::Rotated(new_blob) => {
                self.store
                    .set(self.runtime.id(), account.id.0.as_str(), BLOB_FIELD, &new_blob)?;
                Ok(Some(new_blob))
            }
            RefreshOutcome::DeadToken => {
                tracing::warn!(account = %account.id, "refresh token dead; needs re-login");
                Ok(None)
            }
            RefreshOutcome::Unsupported => Ok(None),
        }
    }
}
```

> 注：`block_in_place` 要求多线程 runtime（本项目 daemon/cli 均是）。若某调用点在 current-thread runtime，改为把 `capture_live_into_store` 也移进后面的 `spawn_blocking`（引擎可持 `Arc<Self>`）。实现时以「Codex 迁移后全部测试通过」为准。

- [ ] **Step 6: 导出 + 写引擎单测（fake runtime）**

在 `lib.rs` 增 `pub mod engine; pub use engine::FileBlobProvider;`。在 `engine.rs` 末尾加 `#[cfg(test)]` 模块，用一个内存 fake runtime 覆盖 capture 守卫三态与 activate 回灌：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use subswap_core::{FileStore};

    struct FakeRuntime {
        home: PathBuf,
    }
    #[async_trait]
    impl FileBlobRuntime for FakeRuntime {
        fn id(&self) -> &'static str { "fake" }
        fn display_name(&self) -> &'static str { "Fake" }
        fn home(&self) -> PathBuf { self.home.clone() }
        fn live_cred_path(&self, home: &Path) -> PathBuf { home.join("live.json") }
        fn parse_metadata(&self, blob: &str) -> BlobMetadata {
            let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
            BlobMetadata {
                primary_id: v.get("uid").and_then(|x| x.as_str()).map(String::from),
                label: v.get("uid").and_then(|x| x.as_str()).map(String::from),
                dedup_key: None,
                extra: serde_json::Map::new(),
            }
        }
        fn isolation(&self) -> IsolationSpec {
            IsolationSpec { env_var: "FAKE_HOME", native_cli: "fake" }
        }
        async fn refresh(&self, _blob: &str) -> Result<RefreshOutcome> {
            Ok(RefreshOutcome::Unsupported)
        }
        async fn fetch_quota(&self, _a: &str, _acc: &Account) -> Result<Vec<Quota>> {
            Ok(vec![])
        }
    }

    fn provider(tmp: &Path) -> FileBlobProvider<FakeRuntime> {
        let store = Arc::new(FileStore::new(tmp.join("creds.json")));
        let registry = Arc::new(AccountRegistry::new(tmp.join("registry.toml")));
        FileBlobProvider::new(FakeRuntime { home: tmp.join("home") }, store, registry)
    }

    #[test]
    fn capture_skips_when_live_missing_refresh_but_store_has_it() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();
        // 注册 owner + store 有 refresh。
        p.import_raw(r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#.into(), None, Some(false)).unwrap();
        // live 缺 refresh。
        fs::write(p.live_path(), r#"{"uid":"u1","access_token":"A2"}"#).unwrap();
        p.reconcile_active_from_live().unwrap();
        let stored = p.store.get("fake", "u1", BLOB_FIELD).unwrap().unwrap();
        assert!(stored.contains("R1"), "store 应保留带 refresh 的旧副本");
    }
}
```

> 该测试用到 `p.store`/`p.live_path()`：把这两个字段/方法在 `#[cfg(test)]` 下设为 `pub(crate)` 可见，或在测试里通过公共方法断言。实现时按最小可见性调整。

- [ ] **Step 7: 运行测试并提交**

Run: `cargo test -p subswap-provider-common`
Expected: PASS。

```bash
git add crates/providers/common/src
git commit -m "feat(common): file-blob provider engine (activate, capture guard, quota refresh)"
```

---

## Phase 2 — Kimi adapter crate `subswap-provider-kimi`

### Task 4: crate 脚手架 + 路径 + 元数据解析（JWT）

**Files:**
- Create: `crates/providers/kimi/Cargo.toml`
- Create: `crates/providers/kimi/src/lib.rs`（暂只 `mod` 声明 + 占位）
- Create: `crates/providers/kimi/src/paths.rs`
- Create: `crates/providers/kimi/src/kimi_files.rs`

**Interfaces:**
- Produces:
  - `paths::kimi_home() -> PathBuf`、`paths::active_cred_path(home: &Path) -> PathBuf`
  - `kimi_files::parse_metadata(blob: &str) -> BlobMetadata`
  - `kimi_files::decode_jwt_payload(token: &str) -> Option<serde_json::Value>`

- [ ] **Step 1: crate 清单** — `crates/providers/kimi/Cargo.toml`

```toml
[package]
name = "subswap-provider-kimi"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
subswap-core = { workspace = true }
subswap-provider-common = { workspace = true }
anyhow = { workspace = true }
async-trait = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
reqwest = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
chrono = { workspace = true }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: paths.rs**

```rust
//! Kimi 本地凭证路径解析。

use std::path::{Path, PathBuf};

/// 解析 Kimi 工作目录：`KIMI_CODE_HOME` > `~/.kimi-code` > `.kimi-code`。
pub fn kimi_home() -> PathBuf {
    if let Ok(v) = std::env::var("KIMI_CODE_HOME") {
        return PathBuf::from(v);
    }
    if let Some(d) = directories::UserDirs::new() {
        return d.home_dir().join(".kimi-code");
    }
    PathBuf::from(".kimi-code")
}

/// 当前激活凭证文件：`<home>/credentials/kimi-code.json`。
pub fn active_cred_path(home: &Path) -> PathBuf {
    home.join("credentials").join("kimi-code.json")
}
```

> 需要 `directories`：在 `Cargo.toml` `[dependencies]` 加 `directories = { workspace = true }`。

- [ ] **Step 3: 写失败测试** — `crates/providers/kimi/src/kimi_files.rs`

真实 access_token 的 payload 含 `user_id` / `client_id` / `scope`。用一个构造的 JWT（payload = `{"user_id":"u-123","client_id":"c-1","scope":"kimi-code"}`）测试：

```rust
//! 解析 kimi-code.json：整段当 opaque blob，只从 access_token JWT 抽 user_id 等做展示。

use subswap_provider_common::BlobMetadata;

/// 解析 base64url JWT payload。
pub fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64url_decode(payload)?;
    serde_json::from_slice(&decoded).ok()
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity((input.len() * 3) / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        while bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// 从 blob 抽元数据。主键 = user_id；label 缺省也用 user_id；无 email。
pub fn parse_metadata(blob: &str) -> BlobMetadata {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(blob) else {
        return BlobMetadata::default();
    };
    let claims = value
        .get("access_token")
        .and_then(|t| t.as_str())
        .and_then(decode_jwt_payload);
    let user_id = claims
        .as_ref()
        .and_then(|c| c.get("user_id"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let mut extra = serde_json::Map::new();
    if let Some(scope) = value.get("scope").and_then(|v| v.as_str()) {
        extra.insert("scope".into(), serde_json::Value::String(scope.into()));
    }

    BlobMetadata {
        primary_id: user_id.clone(),
        label: user_id,
        dedup_key: None,
        extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // header.payload.sig；payload = {"user_id":"u-123","client_id":"c-1","scope":"kimi-code"}
    const JWT: &str = "eyJhbGciOiJFUzI1NiJ9.eyJ1c2VyX2lkIjoidS0xMjMiLCJjbGllbnRfaWQiOiJjLTEiLCJzY29wZSI6ImtpbWktY29kZSJ9.sig";

    #[test]
    fn parses_user_id_from_jwt() {
        let blob = format!(r#"{{"access_token":"{JWT}","scope":"kimi-code"}}"#);
        let m = parse_metadata(&blob);
        assert_eq!(m.primary_id.as_deref(), Some("u-123"));
        assert_eq!(m.label.as_deref(), Some("u-123"));
        assert_eq!(m.extra.get("scope").and_then(|v| v.as_str()), Some("kimi-code"));
    }

    #[test]
    fn garbage_is_empty_metadata() {
        assert!(parse_metadata("not json").primary_id.is_none());
    }
}
```

- [ ] **Step 4: lib.rs 占位声明**

```rust
//! Kimi / Moonshot Provider。基于 subswap-provider-common 的文件型引擎。

mod kimi_files;
mod kimi_usage;
mod oauth;
mod paths;
```

（`kimi_usage`/`oauth` 在 Task 5、6 建；本步可先只声明 `mod kimi_files; mod paths;` 保证编译，后续任务补声明。）

- [ ] **Step 5: 运行测试并提交**

Run: `cargo test -p subswap-provider-kimi`
Expected: PASS（2 个 kimi_files 测试）。

```bash
git add Cargo.toml crates/providers/kimi
git commit -m "feat(kimi): scaffold crate with paths and JWT metadata parser"
```

### Task 5: Kimi OAuth 刷新

**Files:**
- Create: `crates/providers/kimi/src/oauth.rs`
- Modify: `crates/providers/kimi/src/lib.rs`（加 `mod oauth;`）

**Interfaces:**
- Consumes: `kimi_files::decode_jwt_payload`、`common::{extract_refresh_token, RefreshOutcome}`。
- Produces: `pub async fn refresh_blob(blob: &str) -> Result<RefreshOutcome>`。

- [ ] **Step 1: 写 oauth.rs**

```rust
//! Kimi OAuth 刷新：POST {oauth_host}/api/oauth/token（form-urlencoded, grant_type=refresh_token）。

use subswap_core::error::{Error, Result};
use subswap_provider_common::{extract_refresh_token, RefreshOutcome};

use crate::kimi_files::decode_jwt_payload;

/// 解析 OAuth host：`KIMI_CODE_OAUTH_HOST` > `https://auth.kimi.com`。
fn oauth_host() -> String {
    std::env::var("KIMI_CODE_OAUTH_HOST")
        .unwrap_or_else(|_| "https://auth.kimi.com".into())
        .trim_end_matches('/')
        .to_string()
}

/// 从 blob 的 access_token JWT 里取 client_id（刷新请求需要）。
fn client_id_from_blob(blob: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(blob).ok()?;
    let token = v.get("access_token")?.as_str()?;
    decode_jwt_payload(token)?
        .get("client_id")?
        .as_str()
        .map(String::from)
}

/// 用 blob 里的 refresh_token 换新令牌，返回轮换后的完整 blob（合并回原 JSON 结构）。
pub async fn refresh_blob(blob: &str) -> Result<RefreshOutcome> {
    let Some(refresh) = extract_refresh_token(blob) else {
        return Ok(RefreshOutcome::Unsupported);
    };
    let Some(client_id) = client_id_from_blob(blob) else {
        return Ok(RefreshOutcome::Unsupported);
    };

    let url = format!("{}/api/oauth/token", oauth_host());
    let form = [
        ("client_id", client_id.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
    ];

    let resp = reqwest::Client::new()
        .post(&url)
        .header("User-Agent", "subswap")
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("kimi refresh request failed: {e}")))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Ok(RefreshOutcome::DeadToken);
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    if parsed.get("error").and_then(|v| v.as_str()) == Some("invalid_grant") {
        return Ok(RefreshOutcome::DeadToken);
    }
    if !status.is_success() {
        return Err(Error::Provider(format!("kimi refresh HTTP {status}: {body}")));
    }
    let access = parsed.get("access_token").and_then(|v| v.as_str());
    let Some(access) = access else {
        return Err(Error::Provider("kimi refresh response missing access_token".into()));
    };

    // 合并回原 blob 结构，保留未知字段。
    let mut merged: serde_json::Value = serde_json::from_str(blob).unwrap_or(serde_json::json!({}));
    let obj = merged.as_object_mut().unwrap();
    obj.insert("access_token".into(), serde_json::Value::String(access.into()));
    for key in ["refresh_token", "scope", "token_type", "expires_in"] {
        if let Some(v) = parsed.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    if let Some(exp) = parsed.get("expires_in").and_then(|v| v.as_i64()) {
        let now = chrono::Utc::now().timestamp();
        obj.insert("expires_at".into(), serde_json::Value::from(now + exp));
    }
    Ok(RefreshOutcome::Rotated(merged.to_string()))
}
```

- [ ] **Step 2: 声明 mod + 编译**

`lib.rs` 加 `mod oauth;`。Run: `cargo check -p subswap-provider-kimi`。Expected: 通过。

- [ ] **Step 3: 提交**

```bash
git add crates/providers/kimi/src
git commit -m "feat(kimi): OAuth refresh via /api/oauth/token with dead-token detection"
```

### Task 6: Kimi usage 查询与解析

**Files:**
- Create: `crates/providers/kimi/src/kimi_usage.rs`
- Modify: `crates/providers/kimi/src/lib.rs`（加 `mod kimi_usage;`）

**Interfaces:**
- Produces:
  - `pub async fn fetch_quota(access_token: &str, account: &Account) -> Result<Vec<Quota>>`
  - `pub fn parse_usages(body: &str, provider: &str, id: &AccountId) -> Vec<Quota>`（纯函数，供测试）

- [ ] **Step 1: 写解析纯函数 + 测试（真实 payload）**

```rust
//! Kimi usage 查询：GET {base}/usages。数值为字符串、reset 为 ISO8601。
//! usage → 7d 窗口；limits[].window(duration+timeUnit) 换算分钟：300→5h、10080→7d、其余 Custom。

use chrono::{DateTime, Utc};
use subswap_core::error::{Error, Result};
use subswap_core::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};

fn base_url() -> String {
    std::env::var("KIMI_CODE_BASE_URL")
        .unwrap_or_else(|_| "https://api.kimi.com/coding/v1".into())
        .trim_end_matches('/')
        .to_string()
}

fn to_u64(v: Option<&serde_json::Value>) -> Option<u64> {
    let v = v?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.parse().ok())
}

fn reset_at(detail: &serde_json::Value) -> Option<DateTime<Utc>> {
    let s = detail.get("resetTime")?.as_str()?;
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn window_from_minutes(minutes: u64) -> QuotaWindow {
    match minutes {
        300 => QuotaWindow::FiveHour,
        10_080 => QuotaWindow::SevenDay,
        _ => QuotaWindow::Custom,
    }
}

fn minutes_of(window: &serde_json::Value) -> Option<u64> {
    let duration = to_u64(window.get("duration"))?;
    let unit = window.get("timeUnit").and_then(|v| v.as_str()).unwrap_or("");
    let mins = match unit {
        u if u.contains("MINUTE") => duration,
        u if u.contains("HOUR") => duration * 60,
        u if u.contains("DAY") => duration * 60 * 24,
        _ => duration,
    };
    Some(mins)
}

fn quota_from(detail: &serde_json::Value, window: QuotaWindow, provider: &str, id: &AccountId) -> Option<Quota> {
    let limit = to_u64(detail.get("limit"))?;
    let used = to_u64(detail.get("used")).or_else(|| {
        let rem = to_u64(detail.get("remaining"))?;
        Some(limit.saturating_sub(rem))
    })?;
    let pct = if limit > 0 { used as f64 / limit as f64 * 100.0 } else { 0.0 };
    Some(Quota {
        provider: provider.into(),
        account_id: id.clone(),
        window,
        used,
        limit,
        reset_at: reset_at(detail),
        status: QuotaStatus::from_percent(pct),
        note: None,
    })
}

/// 解析 /usages 响应为多窗口 Quota。
pub fn parse_usages(body: &str, provider: &str, id: &AccountId) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Some(usage) = v.get("usage") {
        if let Some(q) = quota_from(usage, QuotaWindow::SevenDay, provider, id) {
            out.push(q);
        }
    }
    if let Some(limits) = v.get("limits").and_then(|x| x.as_array()) {
        for item in limits {
            let window = item
                .get("window")
                .and_then(minutes_of)
                .map(window_from_minutes)
                .unwrap_or(QuotaWindow::Custom);
            let detail = item.get("detail").unwrap_or(item);
            if let Some(q) = quota_from(detail, window, provider, id) {
                out.push(q);
            }
        }
    }
    out
}

/// 调 /usages 端点。
pub async fn fetch_quota(access_token: &str, account: &Account) -> Result<Vec<Quota>> {
    let url = format!("{}/usages", base_url());
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "subswap")
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("kimi usages request failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::QuotaFetch(format!("kimi usages HTTP {status}")));
    }
    Ok(parse_usages(&body, "kimi", &account.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = r#"{
      "usage": {"limit":"100","used":"4","remaining":"96","resetTime":"2026-07-24T07:52:15Z"},
      "limits": [{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
                  "detail":{"limit":"100","used":"18","remaining":"82","resetTime":"2026-07-17T12:52:15Z"}}]
    }"#;

    #[test]
    fn parses_weekly_and_5h() {
        let q = parse_usages(REAL, "kimi", &AccountId("u1".into()));
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].window, QuotaWindow::SevenDay);
        assert_eq!((q[0].used, q[0].limit), (4, 100));
        assert_eq!(q[1].window, QuotaWindow::FiveHour);
        assert_eq!((q[1].used, q[1].limit), (18, 100));
        assert!(q[1].reset_at.is_some());
    }

    #[test]
    fn used_derived_from_remaining_when_absent() {
        let body = r#"{"usage":{"limit":"100","remaining":"70","resetTime":"2026-07-24T07:52:15Z"}}"#;
        let q = parse_usages(body, "kimi", &AccountId("u1".into()));
        assert_eq!((q[0].used, q[0].limit), (30, 100));
    }
}
```

- [ ] **Step 2: 声明 mod、跑测试、提交**

`lib.rs` 加 `mod kimi_usage;`。
Run: `cargo test -p subswap-provider-kimi`。Expected: PASS。

```bash
git add crates/providers/kimi/src
git commit -m "feat(kimi): parse /usages into 5h/7d quota windows"
```

### Task 7: 组装 `KimiProvider`（adapter → 引擎）

**Files:**
- Modify: `crates/providers/kimi/src/lib.rs`

**Interfaces:**
- Produces:
  - `pub const PROVIDER_ID: &str = "kimi";`
  - `pub struct KimiRuntime;`（实现 `FileBlobRuntime`）
  - `pub type KimiProvider = FileBlobProvider<KimiRuntime>;`
  - `pub fn new(store, registry) -> KimiProvider`（便捷构造，与 Codex/Claude 的 `::new` 调用面一致）

- [ ] **Step 1: 写 adapter 与构造函数**

在 `lib.rs`（保留已有 `mod` 声明）追加：

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::{Account, AccountRegistry, CredentialStore, Quota};
use subswap_provider_common::{
    BlobMetadata, FileBlobProvider, FileBlobRuntime, IsolationSpec, RefreshOutcome,
};

pub const PROVIDER_ID: &str = "kimi";

/// Kimi runtime adapter：只提供差异点，机制在引擎。
pub struct KimiRuntime;

#[async_trait]
impl FileBlobRuntime for KimiRuntime {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "Kimi / Moonshot"
    }
    fn home(&self) -> PathBuf {
        paths::kimi_home()
    }
    fn live_cred_path(&self, home: &Path) -> PathBuf {
        paths::active_cred_path(home)
    }
    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        kimi_files::parse_metadata(blob)
    }
    fn isolation(&self) -> IsolationSpec {
        IsolationSpec { env_var: "KIMI_CODE_HOME", native_cli: "kimi" }
    }
    async fn refresh(&self, blob: &str) -> Result<RefreshOutcome> {
        oauth::refresh_blob(blob).await
    }
    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>> {
        kimi_usage::fetch_quota(access_token, account).await
    }
}

/// 便捷别名：Kimi Provider = 文件型引擎 + Kimi adapter。
pub type KimiProvider = FileBlobProvider<KimiRuntime>;

/// 构造 KimiProvider（与 CodexProvider/ClaudeProvider 的 `::new` 调用面一致）。
pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> KimiProvider {
    FileBlobProvider::new(KimiRuntime, store, registry)
}
```

- [ ] **Step 2: 编译 + 测试**

Run: `cargo test -p subswap-provider-kimi`
Expected: PASS（既有 kimi_files/kimi_usage 测试仍通过）。

- [ ] **Step 3: 提交**

```bash
git add crates/providers/kimi/src/lib.rs
git commit -m "feat(kimi): assemble KimiProvider on shared file-blob engine"
```

---

## Phase 3 — 迁移 Codex 到共享引擎

> 硬标准：**Codex 现有全部单测继续通过**，行为零回归。Codex 私有代码（`openai_usage.rs`、
> `codex_files.rs` 的元数据解析、legacy 恢复、`chatgpt_account_id` 去重）保留，只把「机制」换成引擎。

### Task 8a: store 字段兼容 + CodexRuntime adapter

**Files:**
- Modify: `crates/providers/common/src/runtime.rs`（trait 加 `store_field`）
- Modify: `crates/providers/common/src/engine.rs`（用 `runtime.store_field()` 取代常量）
- Create: `crates/providers/codex/src/runtime.rs`
- Modify: `crates/providers/codex/Cargo.toml`（依赖 `subswap-provider-common`）

**Interfaces:**
- Produces: `pub struct CodexRuntime { home: PathBuf }`，实现 `FileBlobRuntime`，`store_field()` 返回 `"auth_json"`（沿用旧字段，免数据迁移）。

- [ ] **Step 1: trait 加 `store_field`（默认 `"blob"`）**

在 `runtime.rs` 的 `FileBlobRuntime` 里增加：

```rust
    /// store 里存 blob 的字段名。默认 "blob"；Codex 为兼容历史数据返回 "auth_json"。
    fn store_field(&self) -> &'static str {
        "blob"
    }
```

- [ ] **Step 2: 引擎改用 `store_field()`**

在 `engine.rs` 里，把所有 `BLOB_FIELD` 出现处（`store.set/get/delete` 的字段参数）改为 `self.runtime.store_field()`；删除 `const BLOB_FIELD`。（`META_DEDUP` 常量保留。）
Run: `cargo test -p subswap-provider-common`。Expected: 仍 PASS（fake runtime 用默认 "blob"）。

- [ ] **Step 3: Codex 依赖 common**

`crates/providers/codex/Cargo.toml` `[dependencies]` 加：

```toml
subswap-provider-common = { workspace = true }
```

- [ ] **Step 4: 写 CodexRuntime（把现有 Codex 逻辑接进 adapter）**

`crates/providers/codex/src/runtime.rs`。元数据映射复用现有 `codex_files::parse_metadata`（返回 `AuthMetadata`），转成 `BlobMetadata`；legacy 恢复与 usage 复用现有函数（从 `lib.rs` 移过来或 `pub(crate)` 暴露）：

```rust
//! Codex runtime adapter：差异点接进共享引擎，Codex 私有逻辑保留。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::{Account, Quota};
use subswap_provider_common::{BlobMetadata, FileBlobRuntime, IsolationSpec, RefreshOutcome};

use crate::codex_files::{parse_metadata as parse_auth_metadata};
use crate::paths::{active_auth_path, codex_home};

pub const PROVIDER_ID: &str = "codex";

pub struct CodexRuntime;

#[async_trait]
impl FileBlobRuntime for CodexRuntime {
    fn id(&self) -> &'static str { PROVIDER_ID }
    fn display_name(&self) -> &'static str { "Codex / ChatGPT" }
    fn store_field(&self) -> &'static str { "auth_json" } // 兼容历史数据
    fn home(&self) -> PathBuf { codex_home() }
    fn live_cred_path(&self, home: &Path) -> PathBuf { active_auth_path(home) }

    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        let m = parse_auth_metadata(blob);
        let mut extra = serde_json::Map::new();
        // 保留供 list 展示与 usage header 使用的字段。
        extra.insert("auth_metadata".into(), serde_json::to_value(&m).unwrap_or_default());
        if let Some(cid) = m.chatgpt_account_id.clone() {
            extra.insert("chatgpt_account_id".into(), serde_json::Value::String(cid));
        }
        BlobMetadata {
            primary_id: m.primary_id(),
            label: m.label(),
            dedup_key: m.chatgpt_account_id.clone(),
            extra,
        }
    }

    fn isolation(&self) -> IsolationSpec {
        IsolationSpec { env_var: "CODEX_HOME", native_cli: "codex" }
    }

    async fn refresh(&self, _blob: &str) -> Result<RefreshOutcome> {
        // Codex 现状不做带外刷新（query_quota 直接用存量 token；401 由上游 CLI 处理）。
        Ok(RefreshOutcome::Unsupported)
    }

    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>> {
        crate::quota::fetch_codex_quota(access_token, account).await
    }

    fn recover_legacy(&self, home: &Path, account: &Account) -> Option<String> {
        crate::legacy::recover_legacy_auth_for_account(home, account)
    }

    fn materialize_extra(&self, _home: &Path, env_dir: &Path) {
        crate::legacy::copy_codex_config_best_effort(env_dir);
    }
}
```

> 迁移动作：把现有 `lib.rs` 里的 `recover_legacy_auth_for_account` / `recover_legacy_auth_from_registry` /
> `copy_*` / `base64_url_no_pad` 等移到新模块 `crate::legacy`，`query_quota` 主体移到 `crate::quota::fetch_codex_quota(access_token, &Account) -> Result<Vec<Quota>>`（内部沿用 `openai_usage` 与 legacy cache 回退，从 `account.extra["chatgpt_account_id"]` 取 header）。这些是「搬运 + 改签名」，逻辑不变。

- [ ] **Step 5: 提交（此时 Codex 尚未切换类型，仅新增 adapter）**

```bash
git add crates/providers/common crates/providers/codex/Cargo.toml crates/providers/codex/src/runtime.rs
git commit -m "refactor(codex): add CodexRuntime adapter over shared engine (not wired yet)"
```

### Task 8b: 切换 `CodexProvider` 类型并保持工作区可编译

**Files:**
- Create: `crates/providers/codex/src/legacy.rs`、`crates/providers/codex/src/quota.rs`
- Modify: `crates/providers/codex/src/lib.rs`（大幅精简为 adapter 组装）
- Modify: `crates/cli/src/cmd/run.rs`、`crates/cli/src/app.rs`、`crates/daemon/src/unix.rs`

**Interfaces:**
- Produces:
  - `pub type CodexProvider = FileBlobProvider<CodexRuntime>;`
  - `pub fn new(store, registry) -> CodexProvider;`
  - 兼容旧调用名的薄封装（见 Step 2）：`export_auth_blob`/`absorb_auth_blob`/`import_raw_with_metadata`。

- [ ] **Step 1: 迁移私有逻辑到 `legacy.rs` / `quota.rs`**

把 `lib.rs` 现有的 `recover_legacy_auth_for_account`、`recover_legacy_auth_from_registry`、
`legacy_account_matches_account`、`base64_url_no_pad`、`copy_codex_config_best_effort`（从 `cli/run.rs` 复制其逻辑）移到 `legacy.rs`（签名改为接收 `home: &Path` / `env_dir: &Path`）；把 `query_quota` 主体（`openai_usage` 调用 + `fresh_cached_legacy_usage` 回退）移到 `quota.rs::fetch_codex_quota(access_token: &str, account: &Account) -> Result<Vec<Quota>>`。保留其**全部现有单测**（迁移到对应模块）。

- [ ] **Step 2: `lib.rs` 精简为组装 + 兼容封装**

```rust
mod codex_files;
mod legacy;
mod openai_usage;
mod paths;
mod quota;
mod runtime;

use std::path::PathBuf;
use std::sync::Arc;

use subswap_core::error::Result;
use subswap_core::{AccountId, AccountRegistry, CredentialStore};
use subswap_provider_common::FileBlobProvider;

pub use runtime::{CodexRuntime, PROVIDER_ID};

pub type CodexProvider = FileBlobProvider<CodexRuntime>;

pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> CodexProvider {
    FileBlobProvider::new(CodexRuntime, store, registry)
}

/// 兼容旧调用名：CLI/daemon 现有代码调用这些。内部转发到引擎方法。
pub trait CodexCompat {
    fn export_auth_blob(&self, id: &AccountId) -> Result<String>;
    fn absorb_auth_blob(&self, id: &AccountId, raw: &str) -> Result<()>;
    fn import_raw_with_metadata(&self, raw: String, _meta: serde_json::Value, active: bool) -> Result<subswap_core::Account>;
}

impl CodexCompat for CodexProvider {
    fn export_auth_blob(&self, id: &AccountId) -> Result<String> {
        self.export_blob(id)
    }
    fn absorb_auth_blob(&self, id: &AccountId, raw: &str) -> Result<()> {
        self.absorb_blob(id, raw)
    }
    fn import_raw_with_metadata(&self, raw: String, _meta: serde_json::Value, active: bool) -> Result<subswap_core::Account> {
        self.import_raw(raw, None, Some(active))
    }
}
```

> `CodexRuntime` 现无字段（`home()` 直接调 `codex_home()`），把 Task 8a 里 `CodexRuntime { home }` 的字段去掉，改无字段结构。

- [ ] **Step 3: 更新构造点与调用名**

- `crates/cli/src/app.rs`：`let codex = Arc::new(CodexProvider::new(...))` → `let codex = Arc::new(subswap_provider_codex::new(store.clone(), registry.clone()));`（类型仍 `Arc<CodexProvider>`）。同法在 `crates/daemon/src/unix.rs`。
- `crates/cli/src/cmd/run.rs`：文件顶部 `use subswap_provider_codex::CodexCompat;`；`ctx.codex.absorb_auth_blob(...)` 与 `export_auth_blob(...)` 保持不变（trait 提供）。`import_raw_with_metadata` 若被 `migrate.rs` 用到同理 `use CodexCompat;`。
- Claude 相关不动。

- [ ] **Step 4: 全量编译 + 测试**

Run: `cargo test --workspace`
Expected: PASS（含迁移过来的 Codex 测试与 common/kimi 测试）。若 `activate` 的 `block_in_place` 在测试 runtime 下 panic，按 Task 3 Step 5 注释改为把 capture 移进 `spawn_blocking`。

- [ ] **Step 5: 提交**

```bash
git add crates/providers/codex crates/cli/src/app.rs crates/cli/src/cmd/run.rs crates/daemon/src/unix.rs
git commit -m "refactor(codex): run CodexProvider on shared engine, keep behavior and tests"
```

---

## Phase 4 — CLI / daemon 接线

### Task 9: 注册 KimiProvider（app + daemon + 默认入口）

**Files:**
- Modify: `crates/cli/Cargo.toml`、`crates/daemon/Cargo.toml`（加 `subswap-provider-kimi`）
- Modify: `crates/cli/src/app.rs`
- Modify: `crates/daemon/src/unix.rs`
- Modify: `crates/cli/src/cmd/default.rs`

**Interfaces:**
- Consumes: `subswap_provider_kimi::{KimiProvider, new as kimi_new}`。
- Produces: `AppContext.kimi: Arc<KimiProvider>`。

- [ ] **Step 1: 加依赖**

`crates/cli/Cargo.toml` 与 `crates/daemon/Cargo.toml` `[dependencies]` 各加：

```toml
subswap-provider-kimi = { workspace = true }
```

- [ ] **Step 2: AppContext 注册**

`crates/cli/src/app.rs`：
- `use subswap_provider_kimi::KimiProvider;`
- 结构体加字段 `pub kimi: Arc<KimiProvider>,`
- `build()` 内：`let kimi = Arc::new(subswap_provider_kimi::new(store.clone(), registry.clone()));`
- `providers.register(kimi.clone());`
- 返回结构体补 `kimi,`。

> `ProviderRegistry` 按 provider id 字母序展开（`list_ordered`）：`claude` < `codex` < `kimi`，Kimi 天然排在末尾，编号一致性无需额外处理。

- [ ] **Step 3: daemon 注册 + reconcile**

`crates/daemon/src/unix.rs`：
- 仿 codex：`let kimi = Arc::new(subswap_provider_kimi::new(store.clone(), registry.clone()));` + `providers.register(kimi.clone());`
- 在做 `claude.reconcile_active_from_live().await` 的那段，追加对 codex、kimi 的 capture-on-arrival：

```rust
if let Err(e) = tokio::task::block_in_place(|| kimi.reconcile_active_from_live()) {
    tracing::debug!(err = %e, "kimi live-credential reconcile skipped");
}
```

> Kimi 不做主动 keepalive（同 Codex）；parked 刷新在 `query_quota` 内按需发生。

- [ ] **Step 4: 默认入口对齐 Kimi active**

`crates/cli/src/cmd/default.rs`：在对 codex 调 `sync_active_metadata(None)` 的位置旁，追加对 kimi 的同样调用（忽略「未登录 Kimi」的错误，与 codex 分支一致的错误处理）：

```rust
match ctx.kimi.sync_active_metadata(None) {
    Ok(_) => {}
    Err(e) => tracing::debug!(err = %e, "kimi active sync skipped"),
}
```

- [ ] **Step 5: 编译 + 测试 + 提交**

Run: `cargo test --workspace`。Expected: PASS。

```bash
git add crates/cli crates/daemon
git commit -m "feat(cli): register Kimi provider in app, daemon reconcile, default entry"
```

### Task 10: `subswap login kimi`（先登再导入）

**Files:**
- Modify: `crates/cli/src/cmd/login.rs`

**Interfaces:**
- Consumes: `ctx.kimi.import_active(None)`、`ctx.registry.set_active("kimi", &id)`。

- [ ] **Step 1: 加 kimi 分支**

在 `login::run` 的 `match provider` 里，`other => bail!(...)` 之前加：

```rust
"kimi" | "moonshot" => {
    if email.is_some() || sso || device_auth {
        bail!("--email/--sso/--device-auth are not supported for kimi login");
    }
    // Kimi 登录是交互式 TUI：约定用户先在 kimi 里登录好，这里只导入当前登录的凭证。
    let account = ctx
        .kimi
        .import_active(None)
        .context("import Kimi login; run `kimi` and sign in first")?;
    ctx.registry
        .set_active("kimi", &account.id)
        .context("mark Kimi login active")?;
    ctx.audit.append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
    println!("login → kimi/{}", account_ref(&account.id.0));
    Ok(())
}
```

- [ ] **Step 2: 更新 usage 文案**

把该文件里 `unknown provider: {other} (expected claude or codex)` 改为 `(expected claude, codex or kimi)`。

- [ ] **Step 3: 编译 + 提交**

Run: `cargo build -p subswap-cli`。Expected: 通过。

```bash
git add crates/cli/src/cmd/login.rs
git commit -m "feat(cli): subswap login kimi imports current logged-in credentials"
```

### Task 11: `subswap run kimi` — 隔离运行改注册表驱动

**Files:**
- Create: `crates/providers/common/src/isolated.rs`
- Modify: `crates/providers/common/src/lib.rs`
- Modify: `crates/cli/src/app.rs`
- Modify: `crates/cli/src/cmd/run.rs`

**Interfaces:**
- Produces（common）：对象安全 trait
  ```rust
  pub trait IsolatedProvider: Send + Sync {
      fn id(&self) -> &'static str;
      fn isolation_env_var(&self) -> &'static str;
      fn native_cli(&self) -> &'static str;
      fn materialize(&self, env_dir: &Path) -> Result<()>; // 写 live 文件到 env_dir + materialize_extra
      fn absorb(&self, id: &AccountId, env_dir: &Path) -> Result<()>;
      fn export_blob(&self, id: &AccountId) -> Result<String>;
  }
  ```
- Produces（app）：`AppContext::isolated(&self, provider_id) -> Option<Arc<dyn IsolatedProvider>>`。

- [ ] **Step 1: common 加 `isolated.rs`**

```rust
//! 隔离运行的对象安全抽象：让 run.rs 不必按 provider 硬编码分支。

use std::path::Path;
use std::sync::Arc;

use subswap_core::error::Result;
use subswap_core::AccountId;

use crate::engine::FileBlobProvider;
use crate::runtime::FileBlobRuntime;

pub trait IsolatedProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn isolation_env_var(&self) -> &'static str;
    fn native_cli(&self) -> &'static str;
    /// 把账号凭证物化进隔离目录：live 文件写到 env_dir 下对应相对位置 + provider 额外物化。
    fn materialize(&self, id: &AccountId, env_dir: &Path) -> Result<()>;
    fn absorb(&self, id: &AccountId, env_dir: &Path) -> Result<()>;
}

impl<A: FileBlobRuntime> IsolatedProvider for FileBlobProvider<A> {
    fn id(&self) -> &'static str {
        Provider_id(self)
    }
    fn isolation_env_var(&self) -> &'static str {
        self.isolation().env_var
    }
    fn native_cli(&self) -> &'static str {
        self.isolation().native_cli
    }
    fn materialize(&self, id: &AccountId, env_dir: &Path) -> Result<()> {
        let blob = self.export_blob(id)?;
        // live 文件相对 home 的子路径（如 credentials/kimi-code.json / auth.json）在 env_dir 下复刻。
        let rel = self.live_rel_path();
        let dest = env_dir.join(rel);
        Self::write_isolated(&dest, &blob)?;
        self.materialize_extra_into(env_dir);
        Ok(())
    }
    fn absorb(&self, id: &AccountId, env_dir: &Path) -> Result<()> {
        let dest = env_dir.join(self.live_rel_path());
        let raw = std::fs::read_to_string(dest)?;
        self.absorb_blob(id, &raw)
    }
}
```

> 需要在 `FileBlobProvider` 上补三个小助手（引擎里加）：
> - `pub fn live_rel_path(&self) -> PathBuf`：`live_cred_path(home)` 去掉 `home` 前缀（用 `strip_prefix`）。
> - `fn write_isolated(path, contents)`：复用 `write_blob`（改 `pub(crate)`）。
> - `fn materialize_extra_into(&self, env_dir)`：转调 `runtime.materialize_extra(&home, env_dir)`。
> - `Provider_id(self)` 用 `self.id()`（`Provider` trait 已实现）；直接写 `self.id()` 即可，去掉占位。

在 `lib.rs` 加 `pub mod isolated; pub use isolated::IsolatedProvider;`。

- [ ] **Step 2: AppContext 提供查表**

`crates/cli/src/app.rs`：加字段
```rust
pub isolated: std::collections::HashMap<&'static str, Arc<dyn subswap_provider_common::IsolatedProvider>>,
```
`build()` 内填入 codex、kimi（claude 不入表，走专用分支）：
```rust
let mut isolated: std::collections::HashMap<&'static str, Arc<dyn IsolatedProvider>> = HashMap::new();
isolated.insert("codex", codex.clone());
isolated.insert("kimi", kimi.clone());
```

- [ ] **Step 3: run.rs 用查表替换 codex 分支**

在 `materialize` / `absorb` / `env_vars` / `primary_env_name` / `native_cli` / `normalize_provider` 里：
- `normalize_provider` 增加 `"kimi" | "moonshot" => Ok("kimi")`，并把错误文案改为 `expected codex, claude or kimi`。
- `materialize`：把 `"codex" => {...}` 分支换成：对 `ctx.isolated.get(acc.provider.as_str())` 命中的走 `iso.materialize(&acc.id, env_dir)`；`"claude"` 保留专用分支；其余 `bail!`。
- `absorb`：同理用 `iso.absorb(...)`；claude 保留。
- `env_vars`/`primary_env_name`/`native_cli`：对表内 provider 用 `iso.isolation_env_var()`/`iso.native_cli()`；claude 保留其 `CLAUDE_CONFIG_DIR`(+macOS SECURESTORAGE) 专用逻辑。

> 结果：新增文件型 runtime 只需在 app.rs 的 `isolated` 表插一行，**不再改 run.rs 分支**。

- [ ] **Step 4: 冒烟测试脚手架**

在 `crates/cli/tests/cli_surface.rs` 的 `isolated_subswap` 里，仿 `CODEX_HOME` 增设 `KIMI_CODE_HOME` 指向一次性临时目录（禁止碰真实 `~/.kimi-code`）。加一个 `run kimi <unknown-id>` 报错路径的用例，确认命令面已注册。

- [ ] **Step 5: 编译 + 测试 + 提交**

Run: `cargo test --workspace`。Expected: PASS。

```bash
git add crates/providers/common crates/cli
git commit -m "feat(cli): registry-driven isolation; subswap run kimi supported"
```

---

## Phase 5 — 文档与发布

### Task 12: 文档同步

**Files:**
- Modify: `AGENTS.md`、`docs/PROVIDER_KNOWLEDGE_BASE.md`、`docs/design/ARCHITECTURE.md`、`docs/CLI.md`

- [ ] **Step 1: AGENTS.md**
  - 「目录速记」加 `crates/providers/common/` 与 `crates/providers/kimi/`。
  - 「文档导航」在 PROVIDER_KNOWLEDGE_BASE 行补上 Kimi/共享引擎适用。
  - 「项目不变量」补一条：文件型 provider 机制在 `crates/providers/common`，新增此类 runtime 只写 adapter + 在 `AppContext::build()`/`isolated` 表注册，不改 `run.rs`/`login.rs` 分支。
  - 「领域地图」Provider 行入口锚点加 `crates/providers/common`。

- [ ] **Step 2: PROVIDER_KNOWLEDGE_BASE.md** 增「Kimi / Moonshot」小节：凭证路径与 `KIMI_CODE_HOME`、令牌 15min/refresh 30d 单次轮换、刷新端点 `POST auth.kimi.com/api/oauth/token`、usage 端点 `/usages` 与 5h/7d 窗口映射、`KIMI_CODE_OAUTH_HOST`/`KIMI_CODE_BASE_URL` 测试重定向。再加「文件型 OAuth 切换共享引擎」小节：引擎职责边界与 adapter trait 差异点表。

- [ ] **Step 3: ARCHITECTURE.md** 增共享引擎分层：`common`(机制) ← `codex`/`kimi`(adapter)；Claude 因钥匙串暂独立。

- [ ] **Step 4: CLI.md** 增 `subswap login kimi`（先登再导入）、`subswap run kimi`（`KIMI_CODE_HOME` 隔离）。

- [ ] **Step 5: 提交**

```bash
git add AGENTS.md docs/
git commit -m "docs: document Kimi provider and shared file-blob engine"
```

### Task 13: 版本、验证与发布

**Files:**
- Modify: `Cargo.toml`（workspace version）、`Cargo.lock`

- [ ] **Step 1: 升版本 + 同步 lock**

`Cargo.toml` `[workspace.package] version` 升 minor（新增 provider，如 `1.0.1` → `1.1.0`）。
Run: `cargo update --workspace --offline`（同步 lock，否则 `--locked` 构建报错）。

- [ ] **Step 2: 全量验证**

```bash
cargo test --workspace
cargo build --workspace
cargo build --locked --release -p subswap-cli -p subswap-daemon
```
Expected: 全部通过。

- [ ] **Step 3: 本机覆盖安装 + daemon 重启 + 冒烟**

```bash
install -m 755 target/release/subswap ~/.local/bin/subswap
install -m 755 target/release/subswapd ~/.local/bin/subswapd
shasum -a 256 target/release/subswap ~/.local/bin/subswap
pkill -f 'subswap __daemon' 2>/dev/null || true; pkill -f 'subswapd' 2>/dev/null || true
SUBSWAP_AUTO_DAEMON=1 ~/.local/bin/subswap        # 默认入口应显示 Kimi 行 + 5h/7d 额度
~/.local/bin/subswap login kimi                   # 导入当前登录的 Kimi 账号
~/.local/bin/subswap                              # 确认 Kimi 出现在编号列表
```

- [ ] **Step 4: 提交、打 tag、推送、确认 Release**

```bash
git add -A && git commit -m "release: vX.Y.0 — add Kimi provider on shared file-blob engine"
git tag vX.Y.0 && git push && git push origin vX.Y.0
```
确认 GitHub Release 发布成功；`update-homebrew.yml` 自动更新 `x0c/homebrew-tap` formula（无需手动）。

---

## Spec 覆盖自检

| Spec 章节 | 对应任务 |
|---|---|
| §3.2 共享引擎位置（common crate） | Task 1 |
| §3.4 adapter trait | Task 2、8a（`store_field`） |
| §3.3 引擎机制（activate/capture 守卫/raw_blob/reconcile/parked 刷新/import/export/absorb） | Task 3 |
| §1 Kimi 凭证/路径/元数据 | Task 4 |
| §1 Kimi 刷新端点 | Task 5 |
| §1 Kimi usage 端点与 5h/7d 映射 | Task 6 |
| §2.1 完整对齐（Provider 组装） | Task 7、9 |
| §2.4 Codex 一起迁移（含 legacy/dedup 钩子） | Task 8a/8b |
| §3.5 注册表驱动 run/login | Task 10、11 |
| §2.2 参与自动换号（5h 窗口 + `AUTO_SWAP_THRESHOLD`） | Task 6（额度）+ Task 9（daemon 注册，走既有 auto_policy，无需专属改动） |
| §2.3 先登再导入 | Task 10 |
| §5 不变量（capture 守卫/只刷 parked/spawn_blocking/TOML null/settings 阈值） | Task 3、6、Global Constraints |
| §6 测试与隔离（KIMI_CODE_HOME/OAUTH_HOST/BASE_URL、Codex 零回归） | Task 3/6/8b/11 |
| §8 发布 | Task 13 |

**自动换号说明**：Kimi 参与自动换号无需改 `auto_policy`——它按 provider 无差别地读各账号 5h 窗口用量与 `AUTO_SWAP_THRESHOLD` 判定；Kimi 只要在 daemon 注册（Task 9）且 `query_quota` 返回 5h 窗口（Task 6）即自动纳入。若实现时发现 `auto_policy` 有 provider 白名单，则在该处补 `"kimi"`（届时新增一步并说明）。

## 执行注意

- 严格 TDD：先跑失败测试再实现。
- Codex 迁移（Task 8）风险最高：以「现有 Codex 测试全绿」为验收硬线，任何行为差异都要回到 `crates/providers/codex` 现有实现比对。
- `activate` 的 `block_in_place` 若在测试 runtime 报错，按 Task 3 Step 5 注释改造（capture 一并进 `spawn_blocking`）。


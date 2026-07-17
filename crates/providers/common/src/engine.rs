//! 文件型 provider 的 provider 无关机制。差异点见 [`crate::FileBlobRuntime`]。
//!
//! 关键约束（与 `crates/providers/codex/src/lib.rs` 保持一致）：
//! - `activate` 不依赖网络；切换 = flock → snapshot 旧文件 → 原子写新 blob → 任一步失败回滚。
//! - `capture_live_into_store`（覆盖 live 前把 live 凭证回灌进 owner 账号 store）必须放在
//!   `spawn_blocking` 内部执行，不能用 `block_in_place`：本引擎的 activate 调用方既可能运行在
//!   多线程 runtime（cli/daemon）也可能是 current-thread runtime（测试），`block_in_place` 在
//!   后者会直接 panic。Codex 现有实现把 capture 调用放进 spawn_blocking 正是为了避开这个问题，
//!   本引擎沿用同样的取舍：`runtime`/`store`/`registry` 都以 `Arc` 形式 clone 进同一个
//!   `spawn_blocking` 闭包。

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

/// 文件型 OAuth 账号切换引擎：接一个 [`FileBlobRuntime`] adapter 即可获得完整 [`Provider`] 实现。
pub struct FileBlobProvider<A: FileBlobRuntime> {
    runtime: Arc<A>,
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    home: PathBuf,
}

// ---------------------------------------------------------------------------
// 构造 / 访问器
// ---------------------------------------------------------------------------

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    pub fn new(runtime: A, store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> Self {
        let runtime = Arc::new(runtime);
        let home = runtime.home();
        Self {
            runtime,
            store,
            registry,
            home,
        }
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

    fn require_account(&self, id: &AccountId) -> Result<Account> {
        self.registry
            .find(self.runtime.id(), id)?
            .ok_or_else(|| Error::AccountNotFound {
                provider: self.runtime.id().into(),
                id: id.to_string(),
            })
    }

    /// 元数据是否指向给定账号：primary_id/label 命中 account.id 或 account.label 任一值，
    /// 或跨主键去重键命中（去重键的 extra 字段名由 `dedup_extra_key` 决定，与原始 Codex
    /// 实现的 `auth_metadata_matches_account`/`string_matches_account` 保持完全一致的比较范围）。
    fn metadata_matches(meta: &BlobMetadata, account: &Account, dedup_extra_key: &str) -> bool {
        let id_hit = meta
            .primary_id
            .as_deref()
            .is_some_and(|v| Self::value_matches_account(v, account))
            || meta
                .label
                .as_deref()
                .is_some_and(|v| Self::value_matches_account(v, account));
        let dedup_hit = match (meta.dedup_key.as_deref(), account.extra.get(dedup_extra_key)) {
            (Some(dk), Some(v)) => v.as_str() == Some(dk),
            _ => false,
        };
        id_hit || dedup_hit
    }

    /// 单个候选值是否命中账号：等于 account.id，或（label 非空时）等于 account.label。
    fn value_matches_account(value: &str, account: &Account) -> bool {
        value == account.id.0 || (!account.label.trim().is_empty() && value == account.label)
    }
}

// ---------------------------------------------------------------------------
// 存账号 / 导入
// ---------------------------------------------------------------------------

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 从 blob 落库：blob 进 store，元数据进 registry.toml。`active_override=None` 时保留原 active。
    /// 元数据由 `self.runtime.parse_metadata(&raw)` 派生；若调用方已持有 metadata（如 legacy
    /// registry 迁移场景），改用 [`Self::store_account_with_metadata`] 跳过重新派生。
    fn store_account(
        &self,
        raw: String,
        label_hint: Option<String>,
        active_override: Option<bool>,
    ) -> Result<Account> {
        let meta = self.runtime.parse_metadata(&raw);
        self.store_account_with_metadata(raw, meta, label_hint, active_override)
    }

    /// 同 [`Self::store_account`]，但接收调用方提供的、已解析好的 metadata，不再从 `raw` 重新派生。
    fn store_account_with_metadata(
        &self,
        raw: String,
        meta: BlobMetadata,
        label_hint: Option<String>,
        active_override: Option<bool>,
    ) -> Result<Account> {
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

        self.store
            .set(self.runtime.id(), id.0.as_str(), self.runtime.store_field(), &raw)?;

        let mut extra = meta.extra.clone();
        if let Some(dk) = meta.dedup_key.clone() {
            extra.insert(
                self.runtime.dedup_extra_key().into(),
                serde_json::Value::String(dk),
            );
        }

        let existing = self
            .registry
            .find(self.runtime.id(), &id)?
            .or_else(|| self.find_by_dedup(&meta));
        if let Some(ex) = existing.as_ref() {
            if ex.id != id {
                let _ = self
                    .store
                    .delete(self.runtime.id(), ex.id.0.as_str(), self.runtime.store_field());
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
        let dedup_extra_key = self.runtime.dedup_extra_key();
        self.registry
            .list_by_provider(self.runtime.id())
            .ok()?
            .into_iter()
            .find(|a| a.extra.get(dedup_extra_key).and_then(|v| v.as_str()) == Some(target))
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
                    .get(self.runtime.id(), pid, self.runtime.store_field())
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

    /// 同 [`Self::import_raw`]，但用调用方提供的 metadata（不重新从 blob 派生）。
    /// 供 legacy registry 迁移场景使用：迁移前的 metadata 可能带有当前 blob 解析逻辑
    /// 推导不出的字段（如缓存的用量、旧 schema 才有的字段），必须原样保留。
    pub fn import_raw_with_explicit_metadata(
        &self,
        raw: String,
        metadata: BlobMetadata,
        active: Option<bool>,
    ) -> Result<Account> {
        self.store_account_with_metadata(raw, metadata, None, active)
    }
}

// ---------------------------------------------------------------------------
// 取 blob / capture-on-leave 守卫 / reconcile / 隔离导出导入
// ---------------------------------------------------------------------------

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 取账号 blob：active 账号优先读 live（并顺手修复 store 副本），parked 读 store，最后 legacy。
    ///
    /// 错误处理顺序与原始 Codex 实现一致：store 读取失败时不立即冒泡，而是先捕获错误、
    /// 继续尝试 `recover_legacy`；只有 legacy 也拿不到时才把原始 store 错误抛出去。这样即使
    /// keyring 后端本身故障，仍有机会从 legacy 布局恢复凭证。
    pub fn raw_blob_for_account(&self, account: &Account) -> Result<String> {
        if let Some(raw) = self.read_live_if_matches(account) {
            let _ = self.store.set(
                self.runtime.id(),
                account.id.0.as_str(),
                self.runtime.store_field(),
                &raw,
            );
            return Ok(raw);
        }
        let store_error = match self.store.get(
            self.runtime.id(),
            account.id.0.as_str(),
            self.runtime.store_field(),
        ) {
            Ok(Some(raw)) => return Ok(raw),
            Ok(None) => None,
            Err(e) => Some(e),
        };
        if let Some(raw) = self.runtime.recover_legacy(&self.home, account) {
            let _ = self.store.set(
                self.runtime.id(),
                account.id.0.as_str(),
                self.runtime.store_field(),
                &raw,
            );
            return Ok(raw);
        }
        if let Some(e) = store_error {
            return Err(e);
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
        Self::metadata_matches(&meta, account, self.runtime.dedup_extra_key()).then_some(raw)
    }

    /// 覆盖 live 前把 live 凭证回灌进其 owner 账号 store（自身走一份，供 `reconcile_active_from_live`
    /// 这类纯同步调用点使用）。
    ///
    /// 守卫：live 缺 refresh 且 store 已有 refresh 时跳过（防静默写死账号）。
    fn capture_live_into_store(&self) -> Result<()> {
        Self::capture_live_into_store_with(
            self.runtime.as_ref(),
            self.store.as_ref(),
            self.registry.as_ref(),
            &self.home,
        )
    }

    /// 与 [`Self::capture_live_into_store`] 逻辑相同，但只接收借用参数，
    /// 便于在 `activate` 的 `spawn_blocking` 闭包内以 clone 出来的 `Arc` 直接调用，
    /// 不必依赖 `&self`（`self` 不是 `'static`，不能安全地移进 `spawn_blocking`）。
    fn capture_live_into_store_with(
        runtime: &A,
        store: &dyn CredentialStore,
        registry: &AccountRegistry,
        home: &Path,
    ) -> Result<()> {
        let live_raw = match fs::read_to_string(runtime.live_cred_path(home)) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        let meta = runtime.parse_metadata(&live_raw);
        let dedup_extra_key = runtime.dedup_extra_key();
        let owner = registry
            .list_by_provider(runtime.id())?
            .into_iter()
            .find(|a| Self::metadata_matches(&meta, a, dedup_extra_key));
        let Some(owner) = owner else { return Ok(()) };

        if extract_refresh_token(&live_raw).is_none() {
            if let Some(existing) = store.get(runtime.id(), owner.id.0.as_str(), runtime.store_field())? {
                if extract_refresh_token(&existing).is_some() {
                    tracing::warn!(
                        account = %owner.id,
                        "live capture missing refresh_token; skipped overwrite to keep existing store copy"
                    );
                    return Ok(());
                }
            }
        }
        store.set(runtime.id(), owner.id.0.as_str(), runtime.store_field(), &live_raw)?;
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
        self.store
            .set(self.runtime.id(), id.0.as_str(), self.runtime.store_field(), raw)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Provider 实现：activate + query_quota
// ---------------------------------------------------------------------------

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
        // 阶段 1：异步预处理（仅查 registry + store，无网络调用），与 Codex 现有实现一致。
        let account = self.require_account(id)?;
        let target_raw = self.raw_blob_for_account(&account)?;

        // 阶段 2：把需要阻塞 IO 的部分（含 capture-on-leave）整体搬进 spawn_blocking。
        // 不用 `block_in_place`：调用方可能运行在 current-thread runtime（如测试），
        // `block_in_place` 在那种 runtime 下会直接 panic；`spawn_blocking` 没有这个限制。
        let home = self.home.clone();
        let live_path = self.live_path();
        let provider_id = self.runtime.id();
        let runtime = self.runtime.clone();
        let store = self.store.clone();
        let registry = self.registry.clone();
        let id_owned = id.clone();

        tokio::task::spawn_blocking(move || {
            // capture-on-leave：覆盖 live 前，先把当前 live 凭证回灌进它所属账号的 store。
            // 否则切走的账号 store 副本会停在旧 refresh token，下次切回写回旧 token → "already used"。
            if let Err(e) = FileBlobProvider::<A>::capture_live_into_store_with(
                runtime.as_ref(),
                store.as_ref(),
                registry.as_ref(),
                &home,
            ) {
                tracing::warn!(err = %e, "capture-on-leave failed; continuing swap");
            }

            let blob = target_raw;
            let targets = vec![SwapTarget {
                snapshot_name: "credentials",
                live_path,
                writer: Box::new(move |p: &Path| FileBlobProvider::<A>::write_blob(p, &blob)),
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
    /// parked 账号刷新一次并写回 store。返回 `Some(新 blob)` 表示已轮换。
    async fn refresh_parked_if_needed(&self, account: &Account, raw: &str) -> Result<Option<String>> {
        match self.runtime.refresh(raw).await? {
            RefreshOutcome::Rotated(new_blob) => {
                self.store.set(
                    self.runtime.id(),
                    account.id.0.as_str(),
                    self.runtime.store_field(),
                    &new_blob,
                )?;
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

// ---------------------------------------------------------------------------
// 测试专用最小可见性访问器（不进正式公共 API）。
// ---------------------------------------------------------------------------

#[cfg(test)]
impl<A: FileBlobRuntime> FileBlobProvider<A> {
    pub(crate) fn test_live_path(&self) -> PathBuf {
        self.live_path()
    }

    pub(crate) fn test_store_get(&self, account_id: &str) -> Option<String> {
        self.store
            .get(self.runtime.id(), account_id, self.runtime.store_field())
            .ok()
            .flatten()
    }

    pub(crate) fn test_runtime(&self) -> &A {
        self.runtime.as_ref()
    }

    pub(crate) fn test_registry_upsert(&self, account: Account) -> Result<()> {
        self.registry.upsert(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use subswap_core::FileStore;

    #[derive(Clone, Copy)]
    enum RefreshBehavior {
        Unsupported,
        Rotate,
        Dead,
    }

    struct FakeRuntime {
        home: PathBuf,
        refresh_behavior: RefreshBehavior,
        refresh_calls: AtomicUsize,
        last_access_token: Mutex<Option<String>>,
        legacy_blob: Option<String>,
        /// 默认 "dedup_key"；部分测试覆盖成非默认名，模拟 Codex 把去重键落在
        /// 一个迁移前就存在、不叫 "dedup_key" 的字段名下。
        dedup_extra_key: &'static str,
    }

    impl FakeRuntime {
        fn new(home: PathBuf) -> Self {
            Self::with_behavior(home, RefreshBehavior::Unsupported)
        }

        fn with_behavior(home: PathBuf, refresh_behavior: RefreshBehavior) -> Self {
            Self {
                home,
                refresh_behavior,
                refresh_calls: AtomicUsize::new(0),
                last_access_token: Mutex::new(None),
                legacy_blob: None,
                dedup_extra_key: "dedup_key",
            }
        }

        fn with_legacy(home: PathBuf, legacy_blob: &str) -> Self {
            Self {
                legacy_blob: Some(legacy_blob.to_string()),
                ..Self::new(home)
            }
        }

        fn with_dedup_extra_key(home: PathBuf, dedup_extra_key: &'static str) -> Self {
            Self {
                dedup_extra_key,
                ..Self::new(home)
            }
        }
    }

    #[async_trait]
    impl FileBlobRuntime for FakeRuntime {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn display_name(&self) -> &'static str {
            "Fake"
        }
        fn home(&self) -> PathBuf {
            self.home.clone()
        }
        fn live_cred_path(&self, home: &Path) -> PathBuf {
            home.join("live.json")
        }
        fn dedup_extra_key(&self) -> &'static str {
            self.dedup_extra_key
        }
        fn parse_metadata(&self, blob: &str) -> BlobMetadata {
            let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
            BlobMetadata {
                primary_id: v.get("uid").and_then(|x| x.as_str()).map(String::from),
                // "label" 字段独立可控，缺省时回退到 uid（沿用既有测试的默认行为）；
                // "dedup" 字段模拟去重键（如 Codex 的 chatgpt_account_id）。
                label: v
                    .get("label")
                    .and_then(|x| x.as_str())
                    .map(String::from)
                    .or_else(|| v.get("uid").and_then(|x| x.as_str()).map(String::from)),
                dedup_key: v.get("dedup").and_then(|x| x.as_str()).map(String::from),
                extra: serde_json::Map::new(),
            }
        }
        fn isolation(&self) -> IsolationSpec {
            IsolationSpec {
                env_var: "FAKE_HOME",
                native_cli: "fake",
            }
        }
        async fn refresh(&self, blob: &str) -> Result<RefreshOutcome> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            Ok(match self.refresh_behavior {
                RefreshBehavior::Unsupported => RefreshOutcome::Unsupported,
                RefreshBehavior::Dead => RefreshOutcome::DeadToken,
                RefreshBehavior::Rotate => {
                    let v: serde_json::Value = serde_json::from_str(blob).unwrap_or_default();
                    let uid = v.get("uid").and_then(|x| x.as_str()).unwrap_or_default();
                    RefreshOutcome::Rotated(format!(r#"{{"uid":"{uid}","access_token":"ROTATED"}}"#))
                }
            })
        }
        async fn fetch_quota(&self, access_token: &str, _account: &Account) -> Result<Vec<Quota>> {
            *self.last_access_token.lock().unwrap() = Some(access_token.to_string());
            Ok(vec![])
        }
        fn recover_legacy(&self, _home: &Path, _account: &Account) -> Option<String> {
            self.legacy_blob.clone()
        }
    }

    /// 包一层 `CredentialStore`，`get` 恒定失败，模拟 keyring 后端故障；`set`/`delete` 照常转发到内层
    /// `FileStore`。用于验证 [`FileBlobProvider::raw_blob_for_account`] 的错误处理顺序：
    /// store 错误不应抢先冒泡，必须先尝试 `recover_legacy`。
    struct FailingGetStore {
        inner: FileStore,
    }

    impl subswap_core::CredentialStore for FailingGetStore {
        fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()> {
            self.inner.set(provider, account, field, value)
        }
        fn get(&self, _provider: &str, _account: &str, _field: &str) -> Result<Option<String>> {
            Err(Error::Credential("simulated store failure".into()))
        }
        fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()> {
            self.inner.delete(provider, account, field)
        }
    }

    fn provider(tmp: &Path) -> FileBlobProvider<FakeRuntime> {
        provider_with(tmp, FakeRuntime::new(tmp.join("home")))
    }

    fn provider_with(tmp: &Path, runtime: FakeRuntime) -> FileBlobProvider<FakeRuntime> {
        let store = Arc::new(FileStore::new(tmp.join("creds.json")));
        let registry = Arc::new(AccountRegistry::new(tmp.join("registry.toml")));
        FileBlobProvider::new(runtime, store, registry)
    }

    fn fake_account(id: &str) -> Account {
        Account {
            provider: "fake".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active: false,
            created_at: chrono::Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    // --- capture-on-leave 守卫三态 ---

    #[test]
    fn capture_skips_when_live_missing_refresh_but_store_has_it() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();
        // 注册 owner + store 有 refresh。
        p.import_raw(
            r#"{"uid":"u1","refresh_token":"R1","access_token":"A1"}"#.into(),
            None,
            Some(false),
        )
        .unwrap();
        // live 缺 refresh。
        fs::write(p.test_live_path(), r#"{"uid":"u1","access_token":"A2"}"#).unwrap();
        p.reconcile_active_from_live().unwrap();
        let stored = p.test_store_get("u1").unwrap();
        assert!(stored.contains("R1"), "store 应保留带 refresh 的旧副本");
    }

    #[test]
    fn capture_overwrites_when_live_has_refresh_and_store_lacks_it() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();
        p.import_raw(r#"{"uid":"u1","access_token":"A1"}"#.into(), None, Some(false))
            .unwrap();
        let live = r#"{"uid":"u1","refresh_token":"R2","access_token":"A2"}"#;
        fs::write(p.test_live_path(), live).unwrap();
        p.reconcile_active_from_live().unwrap();
        let stored = p.test_store_get("u1").unwrap();
        assert_eq!(stored, live, "live 带 refresh 时应正常覆盖 store");
    }

    #[test]
    fn capture_overwrites_when_neither_side_has_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();
        p.import_raw(r#"{"uid":"u1","access_token":"A1"}"#.into(), None, Some(false))
            .unwrap();
        let live = r#"{"uid":"u1","access_token":"A2"}"#;
        fs::write(p.test_live_path(), live).unwrap();
        p.reconcile_active_from_live().unwrap();
        let stored = p.test_store_get("u1").unwrap();
        assert_eq!(stored, live, "两边都没有 refresh 时维持原行为正常覆盖");
    }

    #[test]
    fn capture_skips_when_no_owner_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();
        fs::write(p.test_live_path(), r#"{"uid":"unmanaged"}"#).unwrap();
        // 无匹配账号 → 不写 store。
        p.reconcile_active_from_live().unwrap();
        assert!(p.test_store_get("unmanaged").is_none());
    }

    // --- Finding 1 回归：账号匹配不应比迁移前的 Codex 实现窄 ---

    /// 回归测试：迁移前已存在的账号（`extra` 只有旧键名 "chatgpt_account_id"，没有通用
    /// "dedup_key"）在 primary_id 发生轮换（如 Codex account_key 轮换）后，
    /// capture-on-leave 仍必须能靠去重键（用 runtime 声明的 extra 键名读取）找到 owner，
    /// 否则该账号的 store 副本会停留在旧 refresh token 上，导致下次切回后报
    /// "refresh token already used"。
    ///
    /// 修复前：引擎硬编码只读 `extra["dedup_key"]`，这个键名对该账号不存在 → 找不到 owner → 断言失败。
    #[test]
    fn capture_finds_owner_via_dedup_key_under_runtime_specific_extra_key_name() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_dedup_extra_key(tmp.path().join("home"), "chatgpt_account_id");
        let p = provider_with(tmp.path(), runtime);
        fs::create_dir_all(p.home()).unwrap();

        // 模拟迁移前已存在的账号：id = 旧 account_key，label = 邮箱，
        // extra 只有旧键名 "chatgpt_account_id"，没有 "dedup_key"。
        let mut account = fake_account("old-key");
        account.label = "user@example.com".into();
        account
            .extra
            .insert("chatgpt_account_id".into(), serde_json::Value::String("stable-id".into()));
        p.test_registry_upsert(account).unwrap();

        // live blob：account_key 已轮换成新值，label 缺失，但去重键（dedup）仍是同一个稳定值。
        fs::write(p.test_live_path(), r#"{"uid":"new-key","dedup":"stable-id"}"#).unwrap();
        p.reconcile_active_from_live().unwrap();

        let stored = p
            .test_store_get("old-key")
            .expect("primary_id 轮换后仍应通过去重键找到 owner 并回灌 store");
        assert!(stored.contains("new-key"));
    }

    /// 回归测试：metadata 的 label 字段应对照 account.label 比较（而不是只对照 account.id），
    /// 与迁移前 `string_matches_account` 检查 `account.id.0` 或 `account.label` 的范围一致。
    ///
    /// 修复前：`id_hit` 只把 `meta.label` 与 `account.id` 比较，从不查 `account.label` → 断言失败。
    #[test]
    fn label_metadata_field_matches_account_label_when_primary_id_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();

        let mut account = fake_account("some-id");
        account.label = "user@example.com".into();
        p.test_registry_upsert(account).unwrap();

        // primary_id 与 account.id/account.label 都不命中，但 label 字段命中 account.label。
        fs::write(
            p.test_live_path(),
            r#"{"uid":"unrelated-id","label":"user@example.com"}"#,
        )
        .unwrap();
        p.reconcile_active_from_live().unwrap();

        let stored = p
            .test_store_get("some-id")
            .expect("metadata.label 命中 account.label 时应能找到 owner");
        assert!(stored.contains("unrelated-id"));
    }

    // --- Finding 2 回归：import_raw_with_explicit_metadata 应优先用调用方提供的 metadata ---

    #[test]
    fn import_raw_with_explicit_metadata_prefers_caller_supplied_metadata_over_derived() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());

        let raw = r#"{"uid":"u1","access_token":"A1"}"#.to_string();
        // 显式 metadata 与从 raw 派生的结果不同：primary_id/label 换成别的值，
        // 并带一个 raw 派生不出的 extra 字段（模拟 legacy registry 里缓存的用量字段）。
        let mut extra = serde_json::Map::new();
        extra.insert(
            "cached_field".into(),
            serde_json::Value::String("explicit-value".into()),
        );
        let metadata = BlobMetadata {
            primary_id: Some("explicit-id".into()),
            label: Some("explicit-label".into()),
            dedup_key: None,
            extra,
        };

        let account = p
            .import_raw_with_explicit_metadata(raw, metadata, Some(true))
            .unwrap();
        assert_eq!(
            account.id.0, "explicit-id",
            "应使用显式 metadata 的 primary_id，而不是从 raw 派生的 u1"
        );
        assert_eq!(account.label, "explicit-label");
        assert_eq!(
            account.extra.get("cached_field").and_then(|v| v.as_str()),
            Some("explicit-value"),
            "raw 派生不出的字段应原样保留"
        );
    }

    // --- 导入 / 导出 / 吸收 ---

    #[test]
    fn import_raw_and_export_blob_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        let account = p
            .import_raw(r#"{"uid":"u1","access_token":"A1"}"#.into(), None, Some(true))
            .unwrap();
        assert_eq!(account.id.0, "u1");
        assert!(account.active);
        let exported = p.export_blob(&account.id).unwrap();
        assert!(exported.contains("A1"));
    }

    #[test]
    fn absorb_blob_rejects_invalid_json_and_updates_store_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        let account = p
            .import_raw(r#"{"uid":"u1","access_token":"A1"}"#.into(), None, Some(false))
            .unwrap();

        assert!(p.absorb_blob(&account.id, "not json").is_err());

        p.absorb_blob(&account.id, r#"{"uid":"u1","access_token":"NEW"}"#)
            .unwrap();
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            r#"{"uid":"u1","access_token":"NEW"}"#
        );
    }

    // --- raw_blob_for_account：store 错误处理顺序（不抢先冒泡，先试 legacy）---

    #[test]
    fn raw_blob_for_account_falls_back_to_legacy_when_store_get_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_legacy(
            tmp.path().join("home"),
            r#"{"uid":"u1","access_token":"FROM_LEGACY"}"#,
        );
        let store = Arc::new(FailingGetStore {
            inner: FileStore::new(tmp.path().join("creds.json")),
        });
        let registry = Arc::new(AccountRegistry::new(tmp.path().join("registry.toml")));
        let p = FileBlobProvider::new(runtime, store, registry);
        fs::create_dir_all(p.home()).unwrap();

        let account = fake_account("u1");
        let raw = p.raw_blob_for_account(&account).unwrap();
        assert!(
            raw.contains("FROM_LEGACY"),
            "store.get 失败时应继续尝试 recover_legacy 而不是直接冒泡错误"
        );
        // legacy 命中后应顺手修复 store 副本（即使底层 get 会失败，set 走 FileStore 正常写入）。
        assert!(std::fs::read_to_string(tmp.path().join("creds.json"))
            .unwrap()
            .contains("FROM_LEGACY"));
    }

    #[test]
    fn raw_blob_for_account_surfaces_store_error_when_legacy_also_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::new(tmp.path().join("home")); // legacy_blob = None
        let store = Arc::new(FailingGetStore {
            inner: FileStore::new(tmp.path().join("creds.json")),
        });
        let registry = Arc::new(AccountRegistry::new(tmp.path().join("registry.toml")));
        let p = FileBlobProvider::new(runtime, store, registry);
        fs::create_dir_all(p.home()).unwrap();

        let account = fake_account("u1");
        let err = p.raw_blob_for_account(&account).unwrap_err();
        assert!(
            err.to_string().contains("simulated store failure"),
            "legacy 也拿不到时应把原始 store 错误抛出去，而不是泛化成\"未找到凭证\"：{err}"
        );
    }

    // --- query_quota：parked-only 刷新 ---

    #[tokio::test]
    async fn query_quota_skips_refresh_for_active_account() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let account = p
            .import_raw(r#"{"uid":"u1","access_token":"A1"}"#.into(), None, Some(true))
            .unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            p.test_runtime().last_access_token.lock().unwrap().as_deref(),
            Some("A1")
        );
    }

    #[tokio::test]
    async fn query_quota_refreshes_parked_account_and_persists_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let account = p
            .import_raw(r#"{"uid":"u1","access_token":"OLD"}"#.into(), None, Some(false))
            .unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.test_runtime().last_access_token.lock().unwrap().as_deref(),
            Some("ROTATED")
        );
        let stored = p.test_store_get("u1").unwrap();
        assert!(stored.contains("ROTATED"), "轮换后的 blob 应写回 store");
    }

    #[tokio::test]
    async fn query_quota_dead_token_leaves_store_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Dead);
        let p = provider_with(tmp.path(), runtime);
        let account = p
            .import_raw(r#"{"uid":"u1","access_token":"OLD"}"#.into(), None, Some(false))
            .unwrap();

        // DeadToken 时引擎放弃刷新，继续用旧 access_token 查询（不会因刷新失败而报错）。
        p.query_quota(&account.id).await.unwrap();

        assert_eq!(
            p.test_runtime().last_access_token.lock().unwrap().as_deref(),
            Some("OLD")
        );
        assert_eq!(p.test_store_get("u1").unwrap(), r#"{"uid":"u1","access_token":"OLD"}"#);
    }
}

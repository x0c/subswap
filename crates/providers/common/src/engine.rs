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

/// keyring/FileStore 字段名。整段 blob 存这里。
const BLOB_FIELD: &str = "blob";
/// registry.toml `extra` 里存去重键。
const META_DEDUP: &str = "dedup_key";

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

    /// 元数据是否指向给定账号：命中主键/label，或跨主键去重键。
    fn metadata_matches(meta: &BlobMetadata, account: &Account) -> bool {
        let id_hit = meta.primary_id.as_deref() == Some(account.id.0.as_str())
            || meta.label.as_deref() == Some(account.id.0.as_str());
        let dedup_hit = match (meta.dedup_key.as_deref(), account.extra.get(META_DEDUP)) {
            (Some(dk), Some(v)) => v.as_str() == Some(dk),
            _ => false,
        };
        id_hit || dedup_hit
    }
}

// ---------------------------------------------------------------------------
// 存账号 / 导入
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// 取 blob / capture-on-leave 守卫 / reconcile / 隔离导出导入
// ---------------------------------------------------------------------------

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
        Self::metadata_matches(&meta, account).then_some(raw)
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
        let owner = registry
            .list_by_provider(runtime.id())?
            .into_iter()
            .find(|a| Self::metadata_matches(&meta, a));
        let Some(owner) = owner else { return Ok(()) };

        if extract_refresh_token(&live_raw).is_none() {
            if let Some(existing) = store.get(runtime.id(), owner.id.0.as_str(), BLOB_FIELD)? {
                if extract_refresh_token(&existing).is_some() {
                    tracing::warn!(
                        account = %owner.id,
                        "live capture missing refresh_token; skipped overwrite to keep existing store copy"
                    );
                    return Ok(());
                }
            }
        }
        store.set(runtime.id(), owner.id.0.as_str(), BLOB_FIELD, &live_raw)?;
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
            .get(self.runtime.id(), account_id, BLOB_FIELD)
            .ok()
            .flatten()
    }

    pub(crate) fn test_runtime(&self) -> &A {
        self.runtime.as_ref()
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
    }

    fn provider(tmp: &Path) -> FileBlobProvider<FakeRuntime> {
        provider_with(tmp, FakeRuntime::new(tmp.join("home")))
    }

    fn provider_with(tmp: &Path, runtime: FakeRuntime) -> FileBlobProvider<FakeRuntime> {
        let store = Arc::new(FileStore::new(tmp.join("creds.json")));
        let registry = Arc::new(AccountRegistry::new(tmp.join("registry.toml")));
        FileBlobProvider::new(runtime, store, registry)
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

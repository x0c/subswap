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
use subswap_core::swap::{swap_with_snapshot_prepared, SwapTarget};
use subswap_core::{
    Account, AccountId, AccountRegistry, ClientTarget, CredentialStore, OfficialDisconnect,
    Provider, Quota, RequestCredential,
};

use crate::json::extract_refresh_token;
use crate::runtime::{BlobMetadata, FileBlobRuntime, IsolationSpec, RefreshOutcome};

/// 文件型 OAuth 账号切换引擎：接一个 [`FileBlobRuntime`] adapter 即可获得完整 [`Provider`] 实现。
pub struct FileBlobProvider<A: FileBlobRuntime> {
    runtime: Arc<A>,
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    home: PathBuf,
}

impl<A: FileBlobRuntime> Clone for FileBlobProvider<A> {
    fn clone(&self) -> Self {
        Self {
            runtime: self.runtime.clone(),
            store: self.store.clone(),
            registry: self.registry.clone(),
            home: self.home.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 构造 / 访问器
// ---------------------------------------------------------------------------

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    pub fn new(
        runtime: A,
        store: Arc<dyn CredentialStore>,
        registry: Arc<AccountRegistry>,
    ) -> Self {
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

    /// live 凭证文件相对 `home` 的子路径（隔离物化用：如 Codex 的 `auth.json`、
    /// Kimi 的 `credentials/kimi-code.json`），供 [`crate::isolated::IsolatedProvider`] 通用计算
    /// 隔离目录内的落盘位置，不必按 provider 硬编码具体路径。
    pub fn live_rel_path(&self) -> PathBuf {
        self.live_path()
            .strip_prefix(&self.home)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| self.live_path())
    }

    /// 转调 runtime 的额外物化（如 Codex 复制 `config.toml` 进隔离目录）。
    pub fn materialize_extra_into(&self, id: &AccountId, env_dir: &Path) -> Result<()> {
        let account = self.require_account(id)?;
        self.runtime
            .materialize_extra(&self.home, env_dir, &account)
    }

    /// 隔离目录内 live 文件相对 env_dir 的路径。
    pub fn isolation_dest_rel(&self) -> PathBuf {
        self.runtime
            .isolation_rel_path()
            .unwrap_or_else(|| self.live_rel_path())
    }

    /// 把账号 blob 合进现有 live 内容（默认整文件覆盖）。
    pub fn compose_live_blob(&self, existing: Option<&str>, blob: &str) -> String {
        self.runtime.compose_live(existing, blob)
    }

    /// 隔离目录只物化 runtime 允许的凭证字段；helper-only 账号不复制 Key。
    pub fn isolation_blob_for(&self, id: &AccountId, blob: &str) -> Result<String> {
        let account = self.require_account(id)?;
        Ok(self.runtime.isolation_blob(&account, blob))
    }

    /// 从 live 文件内容抽出本 provider 的 blob（默认整文件）。
    pub fn extract_live_blob(&self, live: &str) -> Option<String> {
        self.runtime.extract_blob(live)
    }

    /// 隔离启动时除 home 环境变量外额外注入的变量。
    pub fn isolation_extra_env(&self, id: &AccountId) -> Vec<(String, String)> {
        match self.export_blob(id) {
            Ok(blob) => {
                let composed = self.runtime.compose_live(None, &blob);
                self.runtime.isolation_extra_env(&composed)
            }
            Err(_) => Vec::new(),
        }
    }

    /// 原子写 live 凭证：tmp + rename + 0o600。同时供隔离物化写入使用（`pub(crate)`）。
    pub(crate) fn write_blob(path: &Path, contents: &str) -> Result<()> {
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
        let dedup_hit = match (
            meta.dedup_key.as_deref(),
            account.extra.get(dedup_extra_key),
        ) {
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

        self.store.set(
            self.runtime.id(),
            id.0.as_str(),
            self.runtime.store_field(),
            &raw,
        )?;

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
                let _ = self.store.delete(
                    self.runtime.id(),
                    ex.id.0.as_str(),
                    self.runtime.store_field(),
                );
                self.registry.remove(self.runtime.id(), &ex.id)?;
            }
        }

        let account = Account {
            provider: self.runtime.id().into(),
            id: id.clone(),
            label,
            active: active_override.unwrap_or_else(|| existing.as_ref().is_some_and(|a| a.active)),
            created_at: existing
                .as_ref()
                .map(|a| a.created_at)
                .unwrap_or_else(Utc::now),
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

    /// 读 live 文件并抽出本 provider 的 blob。
    fn read_extracted_live(&self) -> Result<String> {
        Self::read_extracted_live_at(self.runtime.as_ref(), &self.home)
    }

    fn read_extracted_live_at(runtime: &A, home: &Path) -> Result<String> {
        let live = fs::read_to_string(runtime.live_cred_path(home))
            .map_err(|e| Error::Provider(format!("read live credentials failed: {e}")))?;
        runtime.extract_blob(&live).ok_or_else(|| {
            Error::Provider("live credentials have no usable entry for this provider".into())
        })
    }

    /// 从当前 live 文件导入（可切换）。
    pub fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
        let raw = self.read_extracted_live()?;
        self.store_account(raw, label_hint, Some(true))
    }

    /// 只对齐当前 live 的元数据 active 标记，不写凭证（默认入口用，避免弹钥匙串）。
    pub fn sync_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        if let Some(id) = self.runtime.active_account_override(&self.home) {
            if let Some(account) = self.registry.find(self.runtime.id(), &id)? {
                self.registry.set_active(self.runtime.id(), &id)?;
                return Ok(account);
            }
        }
        let raw = self.read_extracted_live()?;
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
            return self.registry.find(self.runtime.id(), &id)?.ok_or_else(|| {
                Error::AccountNotFound {
                    provider: self.runtime.id().into(),
                    id: id.to_string(),
                }
            });
        }
        self.store_account(raw, label_hint, Some(true))
    }

    /// `rm` 用 async 入口：阻塞部分进 `spawn_blocking`。
    pub async fn disconnect_official(&self, account: &Account) -> Result<OfficialDisconnect> {
        let provider = FileBlobProvider {
            runtime: self.runtime.clone(),
            store: self.store.clone(),
            registry: self.registry.clone(),
            home: self.home.clone(),
        };
        let account = account.clone();
        tokio::task::spawn_blocking(move || provider.disconnect_live_file_if_matches(&account))
            .await
            .map_err(|e| Error::Provider(format!("disconnect join failed: {e}")))?
    }

    /// `rm` 用：删的是当前 live 文件所属账号时，持 `.subswap.lock` 删除 live
    /// 文件（原生登出）；live 是别的账号或根本不存在时返回 `AlreadyGone`，
    /// 调用方直接清本地即可。删除后复核 live 已无该账号，否则报错。
    pub fn disconnect_live_file_if_matches(&self, account: &Account) -> Result<OfficialDisconnect> {
        if self.runtime.disconnect_account(account, &self.home)? {
            return Ok(OfficialDisconnect::Disconnected);
        }
        let live_id = match self.live_account_id() {
            Ok(id) => id,
            Err(_) => return Ok(OfficialDisconnect::AlreadyGone),
        };
        // `live_account_id` 内部已含去重键回退：命中即 registry owner id。
        if live_id != account.id {
            return Ok(OfficialDisconnect::AlreadyGone);
        }
        let live_path = self.live_path();
        let lock_path = self.home.join(".subswap.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::Provider(format!("open lock {}: {e}", lock_path.display())))?;
        fs2::FileExt::lock_exclusive(&lock_file)
            .map_err(|e| Error::Provider(format!("lock credentials: {e}")))?;
        // 锁内重读：并发 activate 可能刚换过号，只删仍属于目标账号的 live。
        let still_target = self
            .live_account_id()
            .map(|id| id == account.id)
            .unwrap_or(false);
        if !still_target {
            return Ok(OfficialDisconnect::AlreadyGone);
        }
        match std::fs::remove_file(&live_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(OfficialDisconnect::AlreadyGone);
            }
            Err(e) => {
                return Err(Error::Provider(format!(
                    "remove live {}: {e}",
                    live_path.display()
                )));
            }
        }
        if self.live_account_id().is_ok() {
            return Err(Error::Provider(
                "live credentials still present after removal; disconnect in the client first"
                    .to_owned(),
            ));
        }
        Ok(OfficialDisconnect::Disconnected)
    }

    /// 当前 live 凭证对应的 registry id。`rm` 用它判断删除的号是否仍在客户端登录着，
    /// 默认入口用它判断「客户端登录着但同步失败」要不要提示。
    pub fn live_account_id(&self) -> Result<AccountId> {
        if let Some(id) = self.runtime.active_account_override(&self.home) {
            if self.registry.find(self.runtime.id(), &id)?.is_some() {
                return Ok(id);
            }
        }
        let raw = self.read_extracted_live()?;
        let meta = self.runtime.parse_metadata(&raw);
        if let Some(existing) = self.find_by_dedup(&meta) {
            return Ok(existing.id);
        }
        let id_string = meta
            .primary_id
            .clone()
            .or(meta.label.clone())
            .ok_or_else(|| {
                Error::Provider("cannot parse account id from live credentials".into())
            })?;
        Ok(AccountId(id_string))
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
        self.raw_blob_for_account_with_repair(account, true)
    }

    /// 只读取账号 blob：与 [`Self::raw_blob_for_account`] 同样的读取顺序，但不写 store。
    /// `request_credential` 投影用，避免只读 IPC 产生副作用。
    fn raw_blob_for_account_readonly(&self, account: &Account) -> Result<String> {
        self.raw_blob_for_account_with_repair(account, false)
    }

    /// `raw_blob_for_account` 的内部实现：`repair=true` 时命中 live/legacy 顺手修复 store 副本
    ///（`activate`/`query_quota`/`export_blob` 用）；`repair=false` 时只读不写
    ///（`request_credential` 投影用）。
    fn raw_blob_for_account_with_repair(&self, account: &Account, repair: bool) -> Result<String> {
        if let Some(raw) = self.read_live_if_matches(account) {
            if repair {
                let _ = self.store.set(
                    self.runtime.id(),
                    account.id.0.as_str(),
                    self.runtime.store_field(),
                    &raw,
                );
            }
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
            if repair {
                let _ = self.store.set(
                    self.runtime.id(),
                    account.id.0.as_str(),
                    self.runtime.store_field(),
                    &raw,
                );
            }
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
        if !self.runtime.uses_live_credentials(account) {
            return None;
        }
        let raw = self.read_extracted_live().ok()?;
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
        let live_raw = match Self::read_extracted_live_at(runtime, home) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        let meta = runtime.parse_metadata(&live_raw);
        let dedup_extra_key = runtime.dedup_extra_key();
        let owner = registry
            .list_by_provider(runtime.id())?
            .into_iter()
            .find(|a| {
                runtime.uses_live_credentials(a)
                    && Self::metadata_matches(&meta, a, dedup_extra_key)
            });
        let Some(owner) = owner else { return Ok(()) };

        if extract_refresh_token(&live_raw).is_none() {
            if let Some(existing) =
                store.get(runtime.id(), owner.id.0.as_str(), runtime.store_field())?
            {
                if extract_refresh_token(&existing).is_some() {
                    tracing::warn!(
                        account = %owner.id,
                        "live capture missing refresh_token; skipped overwrite to keep existing store copy"
                    );
                    return Ok(());
                }
            }
        }
        if runtime.access_token(&live_raw).is_none() {
            if let Some(existing) =
                store.get(runtime.id(), owner.id.0.as_str(), runtime.store_field())?
            {
                if runtime.access_token(&existing).is_some() {
                    tracing::warn!(
                        account = %owner.id,
                        "live capture missing access token; skipped overwrite to keep existing store copy"
                    );
                    return Ok(());
                }
            }
        }
        store.set(
            runtime.id(),
            owner.id.0.as_str(),
            runtime.store_field(),
            &live_raw,
        )?;
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
        let account = self.require_account(id)?;
        if !self.runtime.uses_live_credentials(&account) {
            return Ok(());
        }
        self.store.set(
            self.runtime.id(),
            id.0.as_str(),
            self.runtime.store_field(),
            raw,
        )?;
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
        let account_for_activation = account.clone();

        tokio::task::spawn_blocking(move || {
            swap_with_snapshot_prepared(
                provider_id,
                &home,
                || {
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

                    let (write_credentials, mut targets) =
                        runtime.activation_targets(&account_for_activation, &home)?;
                    if write_credentials {
                        let blob = target_raw;
                        let runtime_for_write = runtime.clone();
                        targets.insert(
                            0,
                            SwapTarget {
                                snapshot_name: "credentials",
                                live_path,
                                writer: Box::new(move |p: &Path| {
                                    let existing = fs::read_to_string(p).ok();
                                    let composed =
                                        runtime_for_write.compose_live(existing.as_deref(), &blob);
                                    FileBlobProvider::<A>::write_blob(p, &composed)
                                }),
                            },
                        );
                    }
                    Ok(targets)
                },
                || registry.set_active(provider_id, &id_owned),
            )
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
        let access = self.runtime.access_token(&raw).ok_or_else(|| {
            Error::QuotaFetch("no access token in credentials; schema may have changed".into())
        })?;
        self.runtime.fetch_quota(&access, &account).await
    }

    async fn request_credential(&self, id: &AccountId) -> Result<Option<RequestCredential>> {
        let account = self.require_account(id)?;
        let provider = self.clone();
        let raw =
            tokio::task::spawn_blocking(move || provider.raw_blob_for_account_readonly(&account))
                .await
                .map_err(|e| {
                    Error::Provider(format!("credential projection join failed: {e}"))
                })??;
        self.runtime.request_credential(&raw)
    }

    async fn renew_credential(&self, id: &AccountId) -> Result<Option<RequestCredential>> {
        let account = self.require_account(id)?;
        let raw = {
            let provider = self.clone();
            let account = account.clone();
            tokio::task::spawn_blocking(move || provider.raw_blob_for_account_readonly(&account))
                .await
                .map_err(|e| Error::Provider(format!("credential projection join failed: {e}")))??
        };
        let current = match self.runtime.request_credential(&raw)? {
            Some(credential) => credential,
            None => return Ok(None),
        };
        if !credential_needs_renewal(current.expires_at_ms) {
            return Ok(Some(current));
        }
        if !account.active {
            // parked：复用既有直连 OAuth 路径；死 token/不支持时返回当前投影，不新增守卫。
            if let Some(fresh) = self.refresh_parked_if_needed(&account, &raw).await? {
                if let Some(credential) = self.runtime.request_credential(&fresh)? {
                    return Ok(Some(credential));
                }
            }
            return Ok(Some(current));
        }
        // active：先确认 live 仍属于该账号，再委托 runtime 官方续期；绝不改 registry active。
        let live_owner_ok = {
            let provider = self.clone();
            let account = account.clone();
            tokio::task::spawn_blocking(move || provider.read_live_if_matches(&account).is_some())
                .await
                .map_err(|e| Error::Provider(format!("credential renewal join failed: {e}")))?
        };
        if !live_owner_ok {
            return Err(Error::Provider(format!(
                "refusing active renewal: live credentials do not match {}:{}",
                self.runtime.id(),
                account.id,
            )));
        }
        match self.runtime.renew_active(&account, &raw).await? {
            RefreshOutcome::Rotated(fresh) => {
                let meta = self.runtime.parse_metadata(&fresh);
                if !Self::metadata_matches(&meta, &account, self.runtime.dedup_extra_key()) {
                    return Err(Error::Provider(format!(
                        "active renewal changed account for {}:{}; refusing to project a different account",
                        self.runtime.id(),
                        account.id,
                    )));
                }
                let access = self.runtime.access_token(&fresh);
                if matches!(access.as_deref(), None | Some("")) {
                    return Err(Error::Provider(format!(
                        "active renewal returned no access token for {}:{}",
                        self.runtime.id(),
                        account.id,
                    )));
                }
                // live 已由官方更新；store 仅在完整 blob 时跟进，避免缺 refresh 回灌写死账号。
                let fresh_has_refresh = extract_refresh_token(&fresh).is_some();
                let old_has_refresh = extract_refresh_token(&raw).is_some();
                if fresh_has_refresh || !old_has_refresh {
                    let _ = self.persist_renewed_blob(&account, fresh.clone()).await;
                } else {
                    tracing::warn!(
                        account = %account.id,
                        "active renewal missing refresh_token; skipped store overwrite"
                    );
                }
                match self.runtime.request_credential(&fresh)? {
                    Some(credential) => Ok(Some(credential)),
                    None => Err(Error::Provider(format!(
                        "renewed credentials unprojectable for {}:{}",
                        self.runtime.id(),
                        account.id,
                    ))),
                }
            }
            RefreshOutcome::DeadToken | RefreshOutcome::Unsupported => Ok(Some(current)),
        }
    }

    fn credential_store_fields(&self) -> &'static [&'static str] {
        self.runtime.credential_store_fields()
    }

    fn post_swap_notice(&self) -> Option<&'static str> {
        self.runtime.post_swap_notice()
    }
}

/// `auth --renew` 是否需要续期：有过期时间且进入 60 秒续期窗（或已过期）才续期；
/// 无过期时间（长期 API key）或仍新鲜时直接返回当前投影，不做网络/子进程工作。
fn credential_needs_renewal(expires_at_ms: Option<i64>) -> bool {
    const RENEW_SKEW_MS: i64 = 60_000;
    match expires_at_ms {
        Some(expires_at_ms) => {
            let now_ms = chrono::Utc::now().timestamp_millis();
            expires_at_ms <= now_ms.saturating_add(RENEW_SKEW_MS)
        }
        None => false,
    }
}

impl<A: FileBlobRuntime> FileBlobProvider<A> {
    /// 续期写回 store：阻塞钥匙串/文件 IO 进 `spawn_blocking`（async 内禁直接阻塞）。
    async fn persist_renewed_blob(&self, account: &Account, blob: String) -> Result<()> {
        let store = self.store.clone();
        let provider_id = self.runtime.id();
        let field = self.runtime.store_field();
        let account_id = account.id.0.clone();
        tokio::task::spawn_blocking(move || {
            store.set(provider_id, account_id.as_str(), field, blob.as_str())
        })
        .await
        .map_err(|e| Error::Provider(format!("credential persist join failed: {e}")))?
    }

    /// parked 账号刷新一次并写回 store。返回 `Some(新 blob)` 表示已轮换。
    async fn refresh_parked_if_needed(
        &self,
        account: &Account,
        raw: &str,
    ) -> Result<Option<String>> {
        match self.runtime.refresh(raw).await? {
            RefreshOutcome::Rotated(new_blob) => {
                self.persist_renewed_blob(account, new_blob.clone()).await?;
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
        renew_active_calls: AtomicUsize,
        renew_active_blob: Mutex<Option<String>>,
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
                renew_active_calls: AtomicUsize::new(0),
                renew_active_blob: Mutex::new(None),
                last_access_token: Mutex::new(None),
                legacy_blob: None,
                dedup_extra_key: "dedup_key",
            }
        }

        fn with_renew_active(home: PathBuf, fresh_blob: &str) -> Self {
            let runtime = Self::new(home);
            *runtime.renew_active_blob.lock().unwrap() = Some(fresh_blob.to_string());
            runtime
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
                    // 保留 `exp`：续期后仍可投影过期时间，不断链。
                    let exp = v
                        .get("exp")
                        .cloned()
                        .unwrap_or(serde_json::json!(9_999_999_999_i64));
                    RefreshOutcome::Rotated(format!(
                        r#"{{"uid":"{uid}","access_token":"ROTATED","exp":{exp}}}"#
                    ))
                }
            })
        }
        async fn renew_active(
            &self,
            _account: &Account,
            _live_blob: &str,
        ) -> Result<RefreshOutcome> {
            self.renew_active_calls.fetch_add(1, Ordering::SeqCst);
            Ok(match self.renew_active_blob.lock().unwrap().clone() {
                Some(blob) => RefreshOutcome::Rotated(blob),
                None => RefreshOutcome::Unsupported,
            })
        }
        fn request_credential(&self, blob: &str) -> Result<Option<RequestCredential>> {
            let v: serde_json::Value = serde_json::from_str(blob).ok().unwrap_or_default();
            let access = match v.get("access_token").and_then(|x| x.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Ok(None),
            };
            let expires_at_ms = v
                .get("exp")
                .and_then(|x| x.as_i64())
                .and_then(|exp| exp.checked_mul(1000));
            Ok(Some(RequestCredential {
                secret: access,
                expires_at_ms,
            }))
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

    fn provider_with<A: FileBlobRuntime>(tmp: &Path, runtime: A) -> FileBlobProvider<A> {
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
        p.import_raw(
            r#"{"uid":"u1","access_token":"A1"}"#.into(),
            None,
            Some(false),
        )
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
        p.import_raw(
            r#"{"uid":"u1","access_token":"A1"}"#.into(),
            None,
            Some(false),
        )
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
        let runtime =
            FakeRuntime::with_dedup_extra_key(tmp.path().join("home"), "chatgpt_account_id");
        let p = provider_with(tmp.path(), runtime);
        fs::create_dir_all(p.home()).unwrap();

        // 模拟迁移前已存在的账号：id = 旧 account_key，label = 邮箱，
        // extra 只有旧键名 "chatgpt_account_id"，没有 "dedup_key"。
        let mut account = fake_account("old-key");
        account.label = "user@example.com".into();
        account.extra.insert(
            "chatgpt_account_id".into(),
            serde_json::Value::String("stable-id".into()),
        );
        p.test_registry_upsert(account).unwrap();

        // live blob：account_key 已轮换成新值，label 缺失，但去重键（dedup）仍是同一个稳定值。
        fs::write(
            p.test_live_path(),
            r#"{"uid":"new-key","dedup":"stable-id"}"#,
        )
        .unwrap();
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
            .import_raw(
                r#"{"uid":"u1","access_token":"A1"}"#.into(),
                None,
                Some(true),
            )
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
            .import_raw(
                r#"{"uid":"u1","access_token":"A1"}"#.into(),
                None,
                Some(false),
            )
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
            .import_raw(
                r#"{"uid":"u1","access_token":"A1"}"#.into(),
                None,
                Some(true),
            )
            .unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("A1")
        );
    }

    #[tokio::test]
    async fn query_quota_refreshes_parked_account_and_persists_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let account = p
            .import_raw(
                r#"{"uid":"u1","access_token":"OLD"}"#.into(),
                None,
                Some(false),
            )
            .unwrap();

        p.query_quota(&account.id).await.unwrap();

        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
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
            .import_raw(
                r#"{"uid":"u1","access_token":"OLD"}"#.into(),
                None,
                Some(false),
            )
            .unwrap();

        // DeadToken 时引擎放弃刷新，继续用旧 access_token 查询（不会因刷新失败而报错）。
        p.query_quota(&account.id).await.unwrap();

        assert_eq!(
            p.test_runtime()
                .last_access_token
                .lock()
                .unwrap()
                .as_deref(),
            Some("OLD")
        );
        assert_eq!(
            p.test_store_get("u1").unwrap(),
            r#"{"uid":"u1","access_token":"OLD"}"#
        );
    }

    #[test]
    fn capture_skips_when_live_missing_access_token_but_store_has_it() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();
        p.import_raw(
            r#"{"uid":"u1","access_token":"A1"}"#.into(),
            None,
            Some(false),
        )
        .unwrap();
        fs::write(p.test_live_path(), r#"{"uid":"u1"}"#).unwrap();
        p.reconcile_active_from_live().unwrap();
        let stored = p.test_store_get("u1").unwrap();
        assert!(
            stored.contains("A1"),
            "store 应保留带 access_token 的旧副本"
        );
    }

    struct SlotRuntime {
        inner: FakeRuntime,
    }

    #[async_trait]
    impl FileBlobRuntime for SlotRuntime {
        fn id(&self) -> &'static str {
            self.inner.id()
        }
        fn display_name(&self) -> &'static str {
            self.inner.display_name()
        }
        fn home(&self) -> PathBuf {
            self.inner.home()
        }
        fn live_cred_path(&self, home: &Path) -> PathBuf {
            self.inner.live_cred_path(home)
        }
        fn parse_metadata(&self, blob: &str) -> BlobMetadata {
            self.inner.parse_metadata(blob)
        }
        fn isolation(&self) -> IsolationSpec {
            self.inner.isolation()
        }
        async fn refresh(&self, blob: &str) -> Result<RefreshOutcome> {
            self.inner.refresh(blob).await
        }
        async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>> {
            self.inner.fetch_quota(access_token, account).await
        }
        fn extract_blob(&self, live_contents: &str) -> Option<String> {
            let v: serde_json::Value = serde_json::from_str(live_contents).ok()?;
            let slot = v.get("slot")?;
            serde_json::to_string(slot).ok()
        }
        fn compose_live(&self, existing_live: Option<&str>, blob: &str) -> String {
            let mut map = existing_live
                .and_then(|s| {
                    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(s).ok()
                })
                .unwrap_or_default();
            let entry = serde_json::from_str(blob).unwrap_or(serde_json::Value::Null);
            map.insert("slot".into(), entry);
            serde_json::Value::Object(map).to_string()
        }
    }

    #[tokio::test]
    async fn import_active_extracts_slot_and_activate_preserves_neighbors() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = SlotRuntime {
            inner: FakeRuntime::new(tmp.path().join("home")),
        };
        let p = provider_with(tmp.path(), runtime);
        fs::create_dir_all(p.home()).unwrap();
        fs::write(
            p.test_live_path(),
            r#"{"keep":"other","slot":{"uid":"u1","access_token":"A1"}}"#,
        )
        .unwrap();

        let account = p.import_active(None).unwrap();
        assert_eq!(account.id.0, "u1");
        let stored: serde_json::Value =
            serde_json::from_str(&p.test_store_get("u1").unwrap()).unwrap();
        assert_eq!(stored["uid"], "u1");
        assert_eq!(stored["access_token"], "A1");
        assert!(stored.get("keep").is_none());

        let parked = p
            .import_raw(
                r#"{"uid":"u2","access_token":"A2"}"#.into(),
                None,
                Some(false),
            )
            .unwrap();
        p.activate(&parked.id).await.unwrap();

        let live: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(p.test_live_path()).unwrap()).unwrap();
        assert_eq!(live["keep"], "other");
        assert_eq!(live["slot"]["uid"], "u2");
        assert_eq!(live["slot"]["access_token"], "A2");
    }

    // --- disconnect_live_file_if_matches：rm 连带原生登出 ---

    #[test]
    fn disconnect_removes_live_file_of_matching_account() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();
        let account = p
            .import_raw(
                r#"{"uid":"u1","access_token":"A1"}"#.into(),
                None,
                Some(true),
            )
            .unwrap();
        fs::write(p.test_live_path(), r#"{"uid":"u1","access_token":"A1"}"#).unwrap();

        let out = p.disconnect_live_file_if_matches(&account).unwrap();
        assert_eq!(out, OfficialDisconnect::Disconnected);
        assert!(!p.test_live_path().exists());
        // 幂等：live 已无，再调一次就是 AlreadyGone。
        assert_eq!(
            p.disconnect_live_file_if_matches(&account).unwrap(),
            OfficialDisconnect::AlreadyGone
        );
    }

    #[test]
    fn disconnect_leaves_live_file_of_other_account_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();
        let other = p
            .import_raw(
                r#"{"uid":"u2","access_token":"A2"}"#.into(),
                None,
                Some(false),
            )
            .unwrap();
        fs::write(p.test_live_path(), r#"{"uid":"u1","access_token":"A1"}"#).unwrap();

        let out = p.disconnect_live_file_if_matches(&other).unwrap();
        assert_eq!(out, OfficialDisconnect::AlreadyGone);
        assert!(p.test_live_path().exists());
    }

    // --- renew_credential：`auth --renew` 契约 ---

    const FRESH_EXP: i64 = 9_999_999_999;
    const STALE_EXP: i64 = 1_000_000_000;

    fn blob_with_exp(uid: &str, access: &str, exp: i64) -> String {
        format!(r#"{{"uid":"{uid}","access_token":"{access}","exp":{exp}}}"#)
    }

    #[tokio::test]
    async fn renew_fresh_parked_noops_without_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let account = p
            .import_raw(blob_with_exp("u1", "FRESH", FRESH_EXP), None, Some(false))
            .unwrap();

        let renewed = p.renew_credential(&account.id).await.unwrap().unwrap();
        assert_eq!(renewed.secret, "FRESH");
        assert_eq!(
            p.test_runtime().refresh_calls.load(Ordering::SeqCst),
            0,
            "新鲜凭证不得触达刷新路径"
        );
        assert_eq!(
            p.test_runtime().renew_active_calls.load(Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn renew_parked_stale_rotates_and_persists_complete_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::with_behavior(tmp.path().join("home"), RefreshBehavior::Rotate);
        let p = provider_with(tmp.path(), runtime);
        let account = p
            .import_raw(blob_with_exp("u1", "OLD", STALE_EXP), None, Some(false))
            .unwrap();

        let renewed = p.renew_credential(&account.id).await.unwrap().unwrap();
        assert_eq!(renewed.secret, "ROTATED");
        assert!(!renewed.secret.contains("refresh"));
        assert_eq!(p.test_runtime().refresh_calls.load(Ordering::SeqCst), 1);
        let stored = p.test_store_get("u1").unwrap();
        assert!(stored.contains("ROTATED"), "轮换后必须写回完整 blob");
        let after: Account = p
            .test_registry_upsert(account.clone())
            .map(|_| p.require_account(&account.id).unwrap())
            .unwrap();
        assert!(!after.active, "续期不得翻转 active 标记");
    }

    #[tokio::test]
    async fn renew_active_unsupported_returns_current_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        fs::create_dir_all(p.home()).unwrap();
        let account = p
            .import_raw(blob_with_exp("u1", "STALE", STALE_EXP), None, Some(true))
            .unwrap();
        fs::write(p.test_live_path(), blob_with_exp("u1", "STALE", STALE_EXP)).unwrap();

        let renewed = p.renew_credential(&account.id).await.unwrap().unwrap();
        assert_eq!(renewed.secret, "STALE");
        assert_eq!(
            p.test_runtime().renew_active_calls.load(Ordering::SeqCst),
            1,
            "active 应委托一次 hook（默认 Unsupported）"
        );
        assert!(p.require_account(&account.id).unwrap().active);
    }

    #[tokio::test]
    async fn renew_active_rotated_validates_owner_and_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = blob_with_exp("u1", "NEW", FRESH_EXP);
        let runtime = FakeRuntime::with_renew_active(tmp.path().join("home"), &fresh);
        let p = provider_with(tmp.path(), runtime);
        fs::create_dir_all(p.home()).unwrap();
        let account = p
            .import_raw(blob_with_exp("u1", "OLD", STALE_EXP), None, Some(true))
            .unwrap();
        fs::write(p.test_live_path(), blob_with_exp("u1", "OLD", STALE_EXP)).unwrap();

        let renewed = p.renew_credential(&account.id).await.unwrap().unwrap();
        assert_eq!(renewed.secret, "NEW");
        assert!(!renewed.secret.contains("refresh"));
        let stored = p.test_store_get("u1").unwrap();
        assert!(stored.contains("NEW"));
        assert!(p.require_account(&account.id).unwrap().active);
    }

    #[tokio::test]
    async fn renew_active_rotated_mismatch_refuses_different_account() {
        let tmp = tempfile::tempdir().unwrap();
        let fresh = blob_with_exp("intruder", "NEW", FRESH_EXP);
        let runtime = FakeRuntime::with_renew_active(tmp.path().join("home"), &fresh);
        let p = provider_with(tmp.path(), runtime);
        fs::create_dir_all(p.home()).unwrap();
        let account = p
            .import_raw(blob_with_exp("u1", "OLD", STALE_EXP), None, Some(true))
            .unwrap();
        fs::write(p.test_live_path(), blob_with_exp("u1", "OLD", STALE_EXP)).unwrap();
        let before_store = p.test_store_get("u1").unwrap();

        let err = p.renew_credential(&account.id).await.unwrap_err();
        assert!(
            err.to_string().contains("different account"),
            "必须拒绝投影另一个账号：{err}"
        );
        assert_eq!(p.test_store_get("u1").unwrap(), before_store);
        assert!(p.require_account(&account.id).unwrap().active);
    }
}

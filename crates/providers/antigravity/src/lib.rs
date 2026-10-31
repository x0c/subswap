//! Antigravity / Gemini Provider：切换 agy 的 OAuth 登录（macOS 钥匙串 `gemini/antigravity`）。
//!
//! 关键约束：
//! - `activate` 不依赖网络；切换 = 读目标 blob → 快照 keychain → 写 keychain → 任一步失败回滚。
//! - macOS 钥匙串经 `/usr/bin/security` 读写（见 `keychain`），禁止 `keyring` crate 写（ACL 中毒）。
//! - refresh_token 为 Google 长效复用 token，`invalid_grant` 才判死；active 只读不刷，parked 按需刷。
//! - 额度主通道 `retrieveUserQuotaSummary`（5h + weekly），无组回落 `fetchAvailableModels`。
//! - 不支持 `run`/`shell`/`env` 隔离（OAuth 在系统钥匙串，无 env 覆盖机制；同 Cursor/Console 边界）。

mod auth;
mod keychain;
mod oauth;
mod paths;
mod quota;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;

use subswap_core::error::{Error, Result};
use subswap_core::{
    Account, AccountId, AccountRegistry, ClientTarget, CredentialStore, OfficialDisconnect,
    Provider, Quota,
};

use crate::auth::{fingerprint, OauthBlob, STORE_FIELD};

/// Provider 标识。
pub const PROVIDER_ID: &str = "antigravity";

#[derive(Clone)]
pub struct AntigravityProvider {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    home: PathBuf,
    /// 已知作废的 refresh 指纹（进程内），止住 daemon 对死 token 的反复刷新。
    dead_refresh: Arc<Mutex<HashSet<String>>>,
}

impl AntigravityProvider {
    pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> Self {
        Self {
            store,
            registry,
            home: paths::antigravity_home(),
            dead_refresh: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn is_refresh_dead(&self, refresh_token: &str) -> bool {
        if refresh_token.is_empty() {
            return false;
        }
        let fp = fingerprint(refresh_token);
        self.dead_refresh
            .lock()
            .map(|s| s.contains(&fp))
            .unwrap_or(false)
    }

    fn mark_refresh_dead(&self, refresh_token: &str) {
        if refresh_token.is_empty() {
            return;
        }
        let fp = fingerprint(refresh_token);
        if let Ok(mut s) = self.dead_refresh.lock() {
            s.insert(fp);
        }
    }

    /// 读 live keychain 并解析为 blob（含 store 回填的 project 透传合并）。
    fn read_live_blob(&self) -> Result<Option<OauthBlob>> {
        let raw = tokio_block_on_keychain_read()?;
        let Some(raw) = raw else { return Ok(None) };
        let Some(mut blob) = keychain::parse_keychain_value(&raw) else {
            return Ok(None);
        };
        // project 上下文只存 store（keychain 无此字段）：按 email 合并回填。
        if let Some(email) = blob.email.clone() {
            if let Ok(Some(stored)) = self
                .store
                .get(PROVIDER_ID, &email, STORE_FIELD)
                .map(|o| o.and_then(|s| OauthBlob::from_blob(&s)))
            {
                if blob.project_id.is_none() {
                    blob.project_id = stored.project_id;
                }
                if blob.managed_project_id.is_none() {
                    blob.managed_project_id = stored.managed_project_id;
                }
            } else if let Ok(Some(by_fp)) = self.find_by_refresh_fp(&blob.refresh_token) {
                let _ = by_fp;
            }
        }
        Ok(Some(blob))
    }

    fn find_by_refresh_fp(&self, _refresh: &str) -> Result<Option<Account>> {
        Ok(None)
    }

    /// 当前 live 对应的 registry id（无 live 时 Err）。
    pub fn live_account_id(&self) -> Result<AccountId> {
        let blob = self.read_live_blob()?.ok_or_else(|| {
            Error::Provider("no Antigravity login; run `agy` and sign in first".into())
        })?;
        blob.primary_id().map(AccountId).ok_or_else(|| {
            Error::Provider("Antigravity live credentials have no usable identity".into())
        })
    }

    /// 由 blob 落库（store + registry）。
    fn store_account(&self, blob: OauthBlob, active_override: Option<bool>) -> Result<Account> {
        let id_str = blob.primary_id().ok_or_else(|| {
            Error::Provider("cannot parse account id from Antigravity credentials".into())
        })?;
        let id = AccountId(id_str);
        let label = blob.label();
        self.store
            .set(PROVIDER_ID, &id.0, STORE_FIELD, &blob.to_blob())?;
        let mut extra = serde_json::Map::new();
        if let Some(email) = blob.email.clone() {
            extra.insert("email".into(), email.into());
        }
        if let Some(p) = blob.project_id.clone() {
            if !p.trim().is_empty() {
                extra.insert("project_id".into(), p.into());
            }
        }
        let existing = self.registry.find(PROVIDER_ID, &id)?;
        let account = Account {
            provider: PROVIDER_ID.into(),
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

    /// 从当前 live 导入（可切换）。
    pub fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
        let blob = self.read_live_blob()?.ok_or_else(|| {
            Error::Provider("no Antigravity login; run `agy` and sign in first".into())
        })?;
        let _ = label_hint;
        // 确保 email 缺失时仍有主键（指纹）。
        if blob.email.is_none() && blob.primary_id().is_none() {
            return Err(Error::Provider(
                "Antigravity live credentials have no usable identity".into(),
            ));
        }
        self.store_account(blob, Some(true))
    }

    /// 只对齐 active 标记，不覆盖 store 已有 blob（默认入口用）。
    pub fn sync_active_metadata(&self, _label_hint: Option<String>) -> Result<Account> {
        let blob = self
            .read_live_blob()?
            .ok_or_else(|| Error::Provider("no Antigravity login".into()))?;
        let id_str = blob.primary_id().ok_or_else(|| {
            Error::Provider("cannot parse account id from Antigravity credentials".into())
        })?;
        let id = AccountId(id_str);
        // 已有可切换副本：只更新 active 标记。
        if self.store.get(PROVIDER_ID, &id.0, STORE_FIELD)?.is_some() {
            self.registry.set_active(PROVIDER_ID, &id)?;
            return self
                .registry
                .find(PROVIDER_ID, &id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                });
        }
        self.store_account(blob, Some(true))
    }

    /// 取账号 blob：active 优先读 live（顺手修复 store），parked 读 store。
    fn blob_for_account(&self, account: &Account) -> Result<OauthBlob> {
        // active 命中 live 即用 live。
        if account.active {
            if let Some(live) = self.read_live_blob()? {
                if live.primary_id().as_deref() == Some(account.id.0.as_str())
                    || live.email.as_deref() == Some(account.id.0.as_str())
                {
                    let _ =
                        self.store
                            .set(PROVIDER_ID, &account.id.0, STORE_FIELD, &live.to_blob());
                    return Ok(live);
                }
            }
        }
        let raw = self
            .store
            .get(PROVIDER_ID, &account.id.0, STORE_FIELD)?
            .ok_or_else(|| {
                Error::Credential(format!(
                    "no stored credentials for {PROVIDER_ID}:{}; run `subswap login antigravity`",
                    account.id
                ))
            })?;
        OauthBlob::from_blob(&raw).ok_or_else(|| {
            Error::Credential(format!(
                "stored {PROVIDER_ID}:{} credentials are corrupt; re-login",
                account.id
            ))
        })
    }

    /// capture-on-leave：覆盖 live 前把 live 回灌进 owner store。
    /// 守卫：live 缺 refresh 且 store 已有 refresh 时跳过（防静默写死）。
    fn capture_live_into_store(&self) -> Result<()> {
        let Some(live) = self.read_live_blob()? else {
            return Ok(());
        };
        if live.refresh_token.trim().is_empty() {
            return Ok(());
        }
        let owner_id = live.primary_id();
        let Some(owner_id) = owner_id else {
            return Ok(());
        };
        let owner = self
            .registry
            .find(PROVIDER_ID, &AccountId(owner_id.clone()))?;
        let Some(owner) = owner else { return Ok(()) };
        // 缺 access 的 live 快照不覆盖 store 完整副本。
        if live.access_token.trim().is_empty() {
            if let Some(existing) = self.store.get(PROVIDER_ID, &owner.id.0, STORE_FIELD)? {
                if OauthBlob::from_blob(&existing)
                    .is_some_and(|b| !b.access_token.trim().is_empty())
                {
                    tracing::warn!(account = %owner.id, "live capture missing access token; skipped overwrite");
                    return Ok(());
                }
            }
        }
        self.store
            .set(PROVIDER_ID, &owner.id.0, STORE_FIELD, &live.to_blob())?;
        Ok(())
    }

    /// daemon capture-on-arrival：只 live→store。
    pub fn reconcile_active_from_live(&self) -> Result<()> {
        self.capture_live_into_store()
    }

    /// parked 保活：临近过期且有 refresh 才刷（active 跳过）。
    pub async fn refresh_if_near_expiry(&self, id: &AccountId) -> Result<bool> {
        let provider = self.clone();
        let account = id.clone();
        let blob = tokio::task::spawn_blocking(move || -> Result<Option<(Account, OauthBlob)>> {
            let acc = provider
                .registry
                .find(PROVIDER_ID, &account)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: account.to_string(),
                })?;
            if acc.active {
                return Ok(None);
            }
            let raw = provider.store.get(PROVIDER_ID, &acc.id.0, STORE_FIELD)?;
            let Some(raw) = raw else { return Ok(None) };
            let Some(b) = OauthBlob::from_blob(&raw) else {
                return Ok(None);
            };
            Ok(Some((acc, b)))
        })
        .await
        .map_err(|e| Error::Provider(format!("read Antigravity keepalive credentials: {e}")))??;
        let Some((_acc, blob)) = blob else {
            return Ok(false);
        };
        if blob.refresh_token.trim().is_empty() || self.is_refresh_dead(&blob.refresh_token) {
            return Ok(false);
        }
        if !blob.access_expired(subswap_core::settings::current().token.refresh_slack_ms) {
            return Ok(false);
        }
        match oauth::refresh_blob(&blob).await? {
            oauth::RefreshOutcome::Refreshed(n) => {
                let changed = n.access_token != blob.access_token;
                self.store
                    .set(PROVIDER_ID, &id.0, STORE_FIELD, &n.to_blob())?;
                Ok(changed)
            }
            oauth::RefreshOutcome::DeadToken(_) => {
                self.mark_refresh_dead(&blob.refresh_token);
                Ok(false)
            }
        }
    }

    /// `rm` 用：删的是当前 live 才清 keychain。
    pub async fn disconnect_official(&self, id: &AccountId) -> Result<OfficialDisconnect> {
        let home = self.home.clone();
        let store = self.store.clone();
        let registry = self.registry.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            let _ = (home, store, registry);
            // 锁内重读归属，避免并发 activate 刚换号。
            match keychain::find_password() {
                Ok(Some(raw)) => {
                    if let Some(live) = keychain::parse_keychain_value(&raw) {
                        let live_id = live.primary_id();
                        if live_id.as_deref() != Some(id.0.as_str()) {
                            return Ok(OfficialDisconnect::AlreadyGone);
                        }
                    } else {
                        return Ok(OfficialDisconnect::AlreadyGone);
                    }
                }
                Ok(None) => return Ok(OfficialDisconnect::AlreadyGone),
                Err(_) => return Ok(OfficialDisconnect::AlreadyGone),
            }
            let lock_path = crate::paths::antigravity_home().join(".subswap.lock");
            let lock_file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|e| Error::Provider(format!("open lock {}: {e}", lock_path.display())))?;
            fs2::FileExt::lock_exclusive(&lock_file)
                .map_err(|e| Error::Provider(format!("lock Antigravity credentials: {e}")))?;
            // 锁内二次确认。
            let still_target = match keychain::find_password() {
                Ok(Some(raw)) => keychain::parse_keychain_value(&raw)
                    .and_then(|b| b.primary_id())
                    .is_some_and(|v| v == id.0),
                _ => false,
            };
            if !still_target {
                return Ok(OfficialDisconnect::AlreadyGone);
            }
            keychain::delete_password()?;
            // 复核已无该账号。
            if let Ok(Some(raw)) = keychain::find_password() {
                if let Some(live) = keychain::parse_keychain_value(&raw) {
                    if live.primary_id().as_deref() == Some(id.0.as_str()) {
                        return Err(Error::Provider(
                            "Antigravity is still signed in after logout; sign out in agy first"
                                .into(),
                        ));
                    }
                }
            }
            Ok(OfficialDisconnect::Disconnected)
        })
        .await
        .map_err(|e| Error::Provider(format!("disconnect join failed: {e}")))?
    }
}

/// keychain 读必须在阻塞线程（`security` 子进程 + 潜在弹窗），但此处只是透传；
/// 真正的 spawn_blocking 由调用方（activate/query_quota 经 Provider async）保障，
/// 同步入口（live_account_id/import）直接调用（CLI 短命，沿用 Claude 同步风格）。
fn tokio_block_on_keychain_read() -> Result<Option<String>> {
    keychain::find_password()
}

#[async_trait]
impl Provider for AntigravityProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "Antigravity / Gemini"
    }

    fn client_targets(&self) -> Vec<ClientTarget> {
        vec![
            ClientTarget {
                id: "antigravity_cli_data".into(),
                display_name: "Antigravity CLI data".into(),
                probe_path: paths::antigravity_home(),
            },
            ClientTarget {
                id: "agy_binary".into(),
                display_name: "agy binary".into(),
                probe_path: PathBuf::from("/opt/homebrew/bin/agy"),
            },
        ]
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(PROVIDER_ID)
    }

    async fn activate(&self, id: &AccountId) -> Result<()> {
        let account =
            self.registry
                .find(PROVIDER_ID, id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                })?;
        let mut target = self.blob_for_account(&account)?;
        // best-effort 预刷新：access 临近过期且有 refresh 时刷一次；失败不阻塞切换。
        if target.access_expired(5 * 60 * 1000)
            && !target.refresh_token.trim().is_empty()
            && !self.is_refresh_dead(&target.refresh_token)
        {
            match oauth::refresh_blob(&target).await {
                Ok(oauth::RefreshOutcome::Refreshed(n)) => {
                    target = n.clone();
                    let _ = self
                        .store
                        .set(PROVIDER_ID, &id.0, STORE_FIELD, &n.to_blob());
                }
                Ok(oauth::RefreshOutcome::DeadToken(_)) => {
                    self.mark_refresh_dead(&target.refresh_token);
                    tracing::warn!(account = %id, "Antigravity refresh token dead; switching with stored access anyway");
                }
                Err(e) => {
                    tracing::warn!(err = %e, account = %id, "Antigravity pre-refresh failed; switching anyway");
                }
            }
        }
        // 重复切当前号：只回灌，不覆写 live。
        if let Ok(live_id) = self.live_account_id() {
            if &live_id == id {
                let s = self.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = s.capture_live_into_store() {
                        tracing::warn!(err = %e, "antigravity active-account capture failed");
                    }
                })
                .await
                .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))?;
                tracing::info!(account = %id, "Antigravity account already active; live credentials preserved");
                return Ok(());
            }
        }
        let home = self.home.clone();
        let store = self.store.clone();
        let registry = self.registry.clone();
        let id_owned = id.clone();
        tokio::task::spawn_blocking(move || {
            // capture-on-leave。
            let tmp = AntigravityProvider {
                store: store.clone(),
                registry: registry.clone(),
                home: home.clone(),
                dead_refresh: Arc::new(Mutex::new(HashSet::new())),
            };
            if let Err(e) = tmp.capture_live_into_store() {
                tracing::warn!(err = %e, "antigravity capture-on-leave failed; continuing swap");
            }
            #[cfg(target_os = "macos")]
            let backup = keychain::snapshot()?;
            #[cfg(not(target_os = "macos"))]
            let backup: Option<String> = None;
            let value = keychain::compose_keychain_value(&target);
            // 先写 keychain（失败直接回滚 registry 不动）。
            #[cfg(target_os = "macos")]
            keychain::set_password(&value)?;
            #[cfg(not(target_os = "macos"))]
            {
                let _ = value;
                return Err(Error::Provider(
                    "Antigravity swap is only supported on macOS in this build".into(),
                ));
            }
            // 再更新 registry active；失败回滚 keychain。
            if let Err(e) = registry.set_active(PROVIDER_ID, &id_owned) {
                #[cfg(target_os = "macos")]
                if let Err(rb) = keychain::restore(backup) {
                    tracing::error!(err = %rb, "Antigravity keychain rollback failed");
                }
                return Err(e);
            }
            // flock 串行化（与其他 subswap 进程互斥），沿用文件型引擎的锁位置。
            let lock_path = home.join(".subswap.lock");
            if let Ok(lock_file) = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
            {
                let _ = fs2::FileExt::lock_exclusive(&lock_file);
                let _ = fs2::FileExt::unlock(&lock_file);
            }
            tracing::info!(account = %id_owned, "Antigravity swap done");
            Ok(())
        })
        .await
        .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))??;
        Ok(())
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        let provider = self.clone();
        let account = id.clone();
        let (blob, is_active) = tokio::task::spawn_blocking(move || {
            let acc = provider
                .registry
                .find(PROVIDER_ID, &account)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: account.to_string(),
                })?;
            let active = acc.active;
            let b = provider.blob_for_account(&acc)?;
            Ok::<_, Error>((b, active))
        })
        .await
        .map_err(|e| Error::Provider(format!("prepare Antigravity usage query: {e}")))??;
        if blob.access_token.trim().is_empty() {
            return Err(Error::QuotaFetch(format!(
                "antigravity {} has no access token; needs re-login",
                id
            )));
        }
        // parked 过期先刷一次（active 只读不刷；401 时 active 也不自愈，交 agy）。
        let mut access = blob.access_token.clone();
        let mut project = blob.managed_project_id.clone().or(blob.project_id.clone());
        if !is_active
            && blob.access_expired(subswap_core::settings::current().token.refresh_slack_ms)
            && !blob.refresh_token.trim().is_empty()
            && !self.is_refresh_dead(&blob.refresh_token)
        {
            match oauth::refresh_blob(&blob).await? {
                oauth::RefreshOutcome::Refreshed(n) => {
                    access = n.access_token.clone();
                    project = n.managed_project_id.clone().or(n.project_id.clone());
                    self.store
                        .set(PROVIDER_ID, &id.0, STORE_FIELD, &n.to_blob())?;
                }
                oauth::RefreshOutcome::DeadToken(msg) => {
                    self.mark_refresh_dead(&blob.refresh_token);
                    return Err(Error::QuotaFetch(format!(
                        "antigravity {id} refresh invalid_grant; needs re-login: {msg}"
                    )));
                }
            }
        }
        let account_obj =
            self.registry
                .find(PROVIDER_ID, id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                })?;
        match quota::fetch_quota(&access, project.as_deref(), &account_obj).await {
            Ok(q) => Ok(q),
            Err(Error::QuotaFetch(msg)) if msg.contains("needs re-login") => {
                Err(Error::QuotaFetch(msg))
            }
            Err(e) => Err(e),
        }
    }

    fn credential_store_fields(&self) -> &'static [&'static str] {
        &[STORE_FIELD]
    }

    fn post_swap_notice(&self) -> Option<&'static str> {
        Some("Restart running agy sessions to use this account.")
    }

    async fn ensure_isolation_supported(&self) -> Result<()> {
        Err(Error::Provider(
            "isolation not supported for provider antigravity (OAuth lives in the system keychain with no env override; use `subswap swap`)".into(),
        ))
    }
}

/// 由 refresh token 直接构造可导入 blob（`login -- <refresh>` 高级用法，占位）。
pub fn blob_from_refresh(refresh: &str, email: Option<String>) -> String {
    OauthBlob {
        refresh_token: refresh.trim().to_string(),
        access_token: String::new(),
        expires_at: None,
        email,
        project_id: None,
        managed_project_id: None,
    }
    .to_blob()
}

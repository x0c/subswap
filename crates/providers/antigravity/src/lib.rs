//! Antigravity / Gemini Provider：切换 agy 的 OAuth 登录（macOS 钥匙串 `gemini/antigravity`）。
//!
//! 关键约束：
//! - `activate` 不依赖网络；切换 = 持锁 capture-on-leave → 快照 keychain → 写 keychain → 更新 registry（失败回滚）。
//! - macOS 钥匙串经 `/usr/bin/security` 读写（见 `keychain`），禁止 `keyring` crate 写（ACL 中毒）。
//! - refresh_token 为 Google 长效复用 token，`invalid_grant` / 401 / 403 才判死；active 只读不刷，parked 按需刷。
//! - 额度主通道 `retrieveUserQuotaSummary`（Gemini + Claude/GPT 双池 × 5h/7d），缺池回落 `fetchAvailableModels`。
//! - 不支持 `run`/`shell`/`env` 隔离，也不实现 `request_credential`（OAuth 在系统钥匙串，且服务条款禁止第三方客户端消费）。

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
        // project 上下文只存 store（keychain 无此字段）：按 primary_id（邮箱或指纹）合并回填。
        if let Some(id) = blob.primary_id() {
            if let Ok(Some(stored)) = self
                .store
                .get(PROVIDER_ID, &id, STORE_FIELD)
                .map(|o| o.and_then(|s| OauthBlob::from_blob(&s)))
            {
                if blob.project_id.is_none() {
                    blob.project_id = stored.project_id;
                }
                if blob.managed_project_id.is_none() {
                    blob.managed_project_id = stored.managed_project_id;
                }
            }
        }
        Ok(Some(blob))
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
        let existing = self.registry.find(PROVIDER_ID, &id)?;
        let mut extra = existing.as_ref().map(|a| a.extra.clone()).unwrap_or_default();
        if let Some(email) = blob.email.clone() {
            extra.insert("email".into(), email.into());
        }
        if let Some(p) = blob.project_id.clone() {
            if !p.trim().is_empty() {
                extra.insert("project_id".into(), p.into());
            }
        }
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
        if active_override == Some(true) {
            self.registry.upsert(account.clone())?;
            self.registry.set_active(PROVIDER_ID, &id)?;
            return self.registry.find(PROVIDER_ID, &id)?.ok_or_else(|| {
                Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                }
            });
        }
        self.registry.upsert(account.clone())?;
        Ok(account)
    }

    /// 从当前 live 导入（可切换）。
    pub fn import_active(&self, _label_hint: Option<String>) -> Result<Account> {
        let blob = self.read_live_blob()?.ok_or_else(|| {
            Error::Provider("no Antigravity login; run `agy` and sign in first".into())
        })?;
        if blob.email.is_none() && blob.primary_id().is_none() {
            return Err(Error::Provider(
                "Antigravity live credentials have no usable identity".into(),
            ));
        }
        self.store_account(blob, Some(true))
    }

    /// 对齐本地当前登录：已有可切换副本则更新 active + 安全回灌，否则导入入池（默认入口用）。
    pub fn sync_active_metadata(&self, _label_hint: Option<String>) -> Result<Account> {
        let blob = self
            .read_live_blob()?
            .ok_or_else(|| Error::Provider("no Antigravity login".into()))?;
        let id_str = blob.primary_id().ok_or_else(|| {
            Error::Provider("cannot parse account id from Antigravity credentials".into())
        })?;
        let id = AccountId(id_str);
        if self.registry.find(PROVIDER_ID, &id)?.is_some()
            && self.store.get(PROVIDER_ID, &id.0, STORE_FIELD)?.is_some()
        {
            self.capture_blob_into_owner(&id, &blob)?;
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

    /// 取账号 blob：active 优先读 live（安全回灌修复 store），parked 读 store。
    fn blob_for_account(&self, account: &Account) -> Result<OauthBlob> {
        if account.active {
            if let Some(live) = self.read_live_blob()? {
                if live.primary_id().as_deref() == Some(account.id.0.as_str())
                    || live.email.as_deref() == Some(account.id.0.as_str())
                {
                    let _ = self.capture_blob_into_owner(&account.id, &live);
                    if !live.access_token.trim().is_empty() {
                        return Ok(live);
                    }
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

    /// 将 live 快照安全回灌到已登记账号的 store（缺 access 时绝不覆盖完整副本）。
    fn capture_blob_into_owner(&self, owner_id: &AccountId, live: &OauthBlob) -> Result<()> {
        if live.refresh_token.trim().is_empty() {
            return Ok(());
        }
        if live.access_token.trim().is_empty() {
            if let Some(existing) = self.store.get(PROVIDER_ID, &owner_id.0, STORE_FIELD)? {
                if OauthBlob::from_blob(&existing)
                    .is_some_and(|b| !b.access_token.trim().is_empty())
                {
                    tracing::warn!(account = %owner_id, "live capture missing access token; skipped overwrite");
                    return Ok(());
                }
            }
        }
        self.store
            .set(PROVIDER_ID, &owner_id.0, STORE_FIELD, &live.to_blob())
    }

    /// capture-on-leave：覆盖 live 前把当前 live 凭证回灌进其归属账号 store。
    fn capture_live_into_store(&self) -> Result<()> {
        let Some(live) = self.read_live_blob()? else {
            return Ok(());
        };
        if live.refresh_token.trim().is_empty() {
            return Ok(());
        }
        let Some(owner_id) = live.primary_id().map(AccountId) else {
            return Ok(());
        };
        if self.registry.find(PROVIDER_ID, &owner_id)?.is_none() {
            return Ok(());
        }
        self.capture_blob_into_owner(&owner_id, &live)
    }

    /// daemon capture-on-arrival：同步当前钥匙串登录。
    /// - 未登录：静默返回 `Ok(())`。
    /// - 已入库账号：安全回灌 live → store，并对齐 active 标记。
    /// - 外部新登录账号：导入入池并标为 active，防止旧 AutoSwap 决策覆盖刚登录的新号。
    pub fn reconcile_active_from_live(&self) -> Result<()> {
        let Some(live) = self.read_live_blob()? else {
            return Ok(());
        };
        if live.refresh_token.trim().is_empty() {
            return Ok(());
        }
        let Some(id_str) = live.primary_id() else {
            return Ok(());
        };
        let id = AccountId(id_str);
        if let Some(existing) = self.registry.find(PROVIDER_ID, &id)? {
            if self.store.get(PROVIDER_ID, &id.0, STORE_FIELD)?.is_some() {
                self.capture_blob_into_owner(&id, &live)?;
                if !existing.active {
                    self.registry.set_active(PROVIDER_ID, &id)?;
                }
                return Ok(());
            }
        }
        let _ = self.store_account(live, Some(true))?;
        Ok(())
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
        match oauth::refresh_blob(&id.0, &blob).await? {
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

    /// `rm` 用：删的是当前 live 才持锁清 keychain。
    pub async fn disconnect_official(&self, id: &AccountId) -> Result<OfficialDisconnect> {
        let home = self.home.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            match keychain::find_password() {
                Ok(Some(raw)) => {
                    if let Some(live) = keychain::parse_keychain_value(&raw) {
                        if live.primary_id().as_deref() != Some(id.0.as_str()) {
                            return Ok(OfficialDisconnect::AlreadyGone);
                        }
                    } else {
                        return Ok(OfficialDisconnect::AlreadyGone);
                    }
                }
                Ok(None) | Err(_) => return Ok(OfficialDisconnect::AlreadyGone),
            }
            std::fs::create_dir_all(&home).map_err(|e| {
                Error::Provider(format!("create Antigravity dir {}: {e}", home.display()))
            })?;
            let lock_path = home.join(".subswap.lock");
            let lock_file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|e| Error::Provider(format!("open lock {}: {e}", lock_path.display())))?;
            fs2::FileExt::lock_exclusive(&lock_file)
                .map_err(|e| Error::Provider(format!("lock Antigravity credentials: {e}")))?;
            // 锁内二次确认归属，避免并发 activate 刚换号。
            let still_target = match keychain::find_password() {
                Ok(Some(raw)) => keychain::parse_keychain_value(&raw)
                    .and_then(|b| b.primary_id())
                    .is_some_and(|v| v == id.0),
                _ => false,
            };
            if !still_target {
                let _ = fs2::FileExt::unlock(&lock_file);
                return Ok(OfficialDisconnect::AlreadyGone);
            }
            keychain::delete_password()?;
            if let Ok(Some(raw)) = keychain::find_password() {
                if let Some(live) = keychain::parse_keychain_value(&raw) {
                    if live.primary_id().as_deref() == Some(id.0.as_str()) {
                        let _ = fs2::FileExt::unlock(&lock_file);
                        return Err(Error::Provider(
                            "Antigravity is still signed in after logout; sign out in agy first"
                                .into(),
                        ));
                    }
                }
            }
            let _ = fs2::FileExt::unlock(&lock_file);
            Ok(OfficialDisconnect::Disconnected)
        })
        .await
        .map_err(|e| Error::Provider(format!("disconnect join failed: {e}")))?
    }
}

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
                probe_path: self.home.clone(),
            },
            ClientTarget {
                id: "agy_binary".into(),
                display_name: "agy binary".into(),
                probe_path: paths::resolve_agy_binary(),
            },
        ]
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(PROVIDER_ID)
    }

    async fn activate(&self, id: &AccountId) -> Result<()> {
        // Antigravity 切换写 macOS 系统钥匙串：非 macOS 构建保持明确不支持。
        // 只读 precedence 检查后直接返回既有错误，不动 registry/store/原生客户端。
        #[cfg(not(target_os = "macos"))]
        {
            if self.registry.find(PROVIDER_ID, id)?.is_none() {
                return Err(Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                });
            }
            return Err(Error::Provider(
                "Antigravity swap is only supported on macOS in this build".into(),
            ));
        }

        // macOS 激活边界（读目标 → best-effort 预刷新 → 持跨进程排他锁执行回灌/快照/写钥匙串/更新 registry）。
        #[cfg(target_os = "macos")]
        {
            let provider_read = self.clone();
            let id_read = id.clone();
            let mut target = tokio::task::spawn_blocking(move || {
                let account = provider_read
                    .registry
                    .find(PROVIDER_ID, &id_read)?
                    .ok_or_else(|| Error::AccountNotFound {
                        provider: PROVIDER_ID.into(),
                        id: id_read.to_string(),
                    })?;
                provider_read.blob_for_account(&account)
            })
            .await
            .map_err(|e| Error::Provider(format!("read Antigravity target failed: {e}")))??;

            // best-effort 预刷新：access 临近过期且有 refresh 时刷一次；失败不阻塞手动切换。
            if target.access_expired(5 * 60 * 1000)
                && !target.refresh_token.trim().is_empty()
                && !self.is_refresh_dead(&target.refresh_token)
            {
                match oauth::refresh_blob(&id.0, &target).await {
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

            let provider = self.clone();
            let id_owned = id.clone();
            tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(&provider.home).map_err(|e| {
                    Error::Provider(format!(
                        "create Antigravity dir {}: {e}",
                        provider.home.display()
                    ))
                })?;
                let lock_path = provider.home.join(".subswap.lock");
                let lock_file = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(false)
                    .open(&lock_path)
                    .map_err(|e| {
                        Error::Provider(format!("open lock {}: {e}", lock_path.display()))
                    })?;
                fs2::FileExt::lock_exclusive(&lock_file)
                    .map_err(|e| Error::Provider(format!("lock Antigravity credentials: {e}")))?;

                let result = (|| -> Result<()> {
                    // 重复切当前 live 账号：安全回灌并对齐 registry active，不覆写 live。
                    if let Ok(live_id) = provider.live_account_id() {
                        if live_id == id_owned {
                            if let Err(e) = provider.capture_live_into_store() {
                                tracing::warn!(err = %e, "antigravity active-account capture failed");
                            }
                            provider.registry.set_active(PROVIDER_ID, &id_owned)?;
                            tracing::info!(account = %id_owned, "Antigravity account already active; live credentials preserved");
                            return Ok(());
                        }
                    }

                    if let Err(e) = provider.capture_live_into_store() {
                        tracing::warn!(err = %e, "antigravity capture-on-leave failed; continuing swap");
                    }
                    let backup = keychain::snapshot()?;
                    let value = keychain::compose_keychain_value(&target);
                    keychain::set_password(&value)?;
                    if let Err(e) = provider.registry.set_active(PROVIDER_ID, &id_owned) {
                        if let Err(rb) = keychain::restore(backup) {
                            tracing::error!(err = %rb, "Antigravity keychain rollback failed");
                        }
                        return Err(e);
                    }
                    tracing::info!(account = %id_owned, "Antigravity swap done");
                    Ok(())
                })();

                let _ = fs2::FileExt::unlock(&lock_file);
                result
            })
            .await
            .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))??;
            Ok(())
        }
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        let provider = self.clone();
        let account = id.clone();
        let (acc_obj, blob, default_proj) = tokio::task::spawn_blocking(move || {
            let acc = provider
                .registry
                .find(PROVIDER_ID, &account)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: account.to_string(),
                })?;
            let b = provider.blob_for_account(&acc)?;
            let dp = paths::default_project_id(&provider.home);
            Ok::<_, Error>((acc, b, dp))
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
        let mut project = blob
            .managed_project_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| blob.project_id.clone().filter(|s| !s.trim().is_empty()));

        if !acc_obj.active
            && blob.access_expired(subswap_core::settings::current().token.refresh_slack_ms)
            && !blob.refresh_token.trim().is_empty()
            && !self.is_refresh_dead(&blob.refresh_token)
        {
            match oauth::refresh_blob(&id.0, &blob).await? {
                oauth::RefreshOutcome::Refreshed(n) => {
                    access = n.access_token.clone();
                    project = n
                        .managed_project_id
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .or_else(|| n.project_id.clone().filter(|s| !s.trim().is_empty()));
                    let store = self.store.clone();
                    let id_str = id.0.clone();
                    let encoded = n.to_blob();
                    let _ = tokio::task::spawn_blocking(move || {
                        store.set(PROVIDER_ID, &id_str, STORE_FIELD, &encoded)
                    })
                    .await;
                }
                oauth::RefreshOutcome::DeadToken(msg) => {
                    self.mark_refresh_dead(&blob.refresh_token);
                    return Err(Error::QuotaFetch(format!(
                        "antigravity {id} refresh invalid_grant; needs re-login: {msg}"
                    )));
                }
            }
        }
        let effective_project = project.or(default_proj);
        quota::fetch_quota(
            &access,
            effective_project.as_deref(),
            blob.auth_method.as_deref(),
            &acc_obj,
        )
        .await
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

#[cfg(test)]
mod capture_guard_tests {
    use super::*;

    #[test]
    fn capture_guard_never_clobbers_stored_access_with_empty_live_access() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(subswap_core::FileStore::new(
            tmp.path().join("credentials.json"),
        ));
        let registry = Arc::new(AccountRegistry::new(tmp.path().join("registry.toml")));
        let provider = AntigravityProvider {
            store: store.clone(),
            registry: registry.clone(),
            home: tmp.path().join("agy"),
            dead_refresh: Arc::new(Mutex::new(HashSet::new())),
        };
        let full = OauthBlob {
            refresh_token: "1//full".into(),
            access_token: "ya29.valid".into(),
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            email: Some("guard@example.com".into()),
            auth_method: Some("consumer".into()),
            project_id: None,
            managed_project_id: None,
            native_extra: Default::default(),
        };
        let acc = provider.store_account(full, Some(true)).unwrap();

        let partial = OauthBlob {
            refresh_token: "1//full".into(),
            access_token: String::new(),
            expires_at: None,
            email: Some("guard@example.com".into()),
            auth_method: Some("consumer".into()),
            project_id: None,
            managed_project_id: None,
            native_extra: Default::default(),
        };
        provider
            .capture_blob_into_owner(&acc.id, &partial)
            .unwrap();

        let kept = OauthBlob::from_blob(
            &store
                .get(PROVIDER_ID, &acc.id.0, STORE_FIELD)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(kept.access_token, "ya29.valid");
    }
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod macos_reconcile_tests {
    use super::*;

    static KEYCHAIN_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn reconcile_imports_untracked_live_login_in_isolated_keychain() {
        let _guard = KEYCHAIN_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let kc_path = tmp.path().join("test.keychain-db");
        let _ = std::process::Command::new("/usr/bin/security")
            .args(["create-keychain", "-p", ""])
            .arg(&kc_path)
            .status();
        std::env::set_var("SUBSWAP_ANTIGRAVITY_KEYCHAIN_PATH", &kc_path);

        let blob = OauthBlob {
            refresh_token: "1//live-new".into(),
            access_token: "ya29.live-new".into(),
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            email: Some("newlogin@example.com".into()),
            auth_method: Some("consumer".into()),
            project_id: None,
            managed_project_id: None,
            native_extra: Default::default(),
        };
        keychain::set_password(&keychain::compose_keychain_value(&blob)).unwrap();

        let store = Arc::new(subswap_core::FileStore::new(
            tmp.path().join("credentials.json"),
        ));
        let registry = Arc::new(AccountRegistry::new(tmp.path().join("registry.toml")));
        let provider = AntigravityProvider {
            store: store.clone(),
            registry: registry.clone(),
            home: tmp.path().join("agy"),
            dead_refresh: Arc::new(Mutex::new(HashSet::new())),
        };

        provider.reconcile_active_from_live().unwrap();
        let imported = registry
            .find(PROVIDER_ID, &AccountId("newlogin@example.com".into()))
            .unwrap()
            .expect("untracked live login must be imported");
        assert!(imported.active);
        assert!(store
            .get(PROVIDER_ID, "newlogin@example.com", STORE_FIELD)
            .unwrap()
            .is_some());

        let _ = std::process::Command::new("/usr/bin/security")
            .arg("delete-keychain")
            .arg(&kc_path)
            .status();
        std::env::remove_var("SUBSWAP_ANTIGRAVITY_KEYCHAIN_PATH");
    }
}

/// 非 macOS 回归：`activate` 必须返回既有不支持错误，且不动 registry/store/客户端。
/// fresh access fixture 验证平台边界本身；expired parked fixture 验证即使满足
/// pre-refresh 条件也不触网、不轮换 token。
#[cfg(test)]
#[cfg(not(target_os = "macos"))]
mod non_mac_activation_tests {
    use super::*;

    fn fixture_with_expiry(
        tmp: &tempfile::TempDir,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> (AntigravityProvider, AccountId, String) {
        let store = Arc::new(subswap_core::FileStore::new(
            tmp.path().join("credentials.json"),
        ));
        let registry = Arc::new(AccountRegistry::new(tmp.path().join("registry.toml")));
        let provider = AntigravityProvider::new(store.clone(), registry.clone());
        let blob = OauthBlob {
            refresh_token: "1//fixture-refresh".into(),
            access_token: "ya29.fixture".into(),
            expires_at: Some(expires_at),
            email: Some("fixture@example.com".into()),
            auth_method: None,
            project_id: None,
            managed_project_id: None,
            native_extra: Default::default(),
        };
        let raw = blob.to_blob();
        let id = AccountId("fixture@example.com".into());
        store.set(PROVIDER_ID, &id.0, STORE_FIELD, &raw).unwrap();
        registry
            .upsert(Account {
                provider: PROVIDER_ID.into(),
                id: id.clone(),
                label: "fixture@example.com".into(),
                active: false,
                created_at: chrono::Utc::now(),
                last_used_at: None,
                priority: 100,
                extra: serde_json::Map::new(),
            })
            .unwrap();
        (provider, id, raw)
    }

    fn fixture(tmp: &tempfile::TempDir) -> (AntigravityProvider, AccountId, String) {
        fixture_with_expiry(tmp, chrono::Utc::now() + chrono::Duration::hours(2))
    }

    #[tokio::test]
    async fn unsupported_activation_returns_existing_error_without_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let (provider, id, raw) = fixture(&tmp);
        let err = provider.activate(&id).await.unwrap_err().to_string();
        assert!(
            err.contains("only supported on macOS"),
            "must keep the existing explicit error, got: {err}"
        );
        // registry 不动：账号仍在且保持 parked。
        let kept = provider.registry.find(PROVIDER_ID, &id).unwrap().unwrap();
        assert!(!kept.active, "unsupported swap must not flip active");
        // store 不动：blob 原样。
        let kept_raw = provider
            .store
            .get(PROVIDER_ID, &id.0, STORE_FIELD)
            .unwrap()
            .unwrap();
        assert_eq!(kept_raw, raw, "unsupported swap must not touch store");
        // 客户端不动：桩 keychain 始终无 live。
        assert!(crate::keychain::find_password().unwrap().is_none());
    }

    #[tokio::test]
    async fn unsupported_activation_keeps_account_not_found_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let (provider, _, _) = fixture(&tmp);
        let ghost = AccountId("ghost@example.com".into());
        let err = provider.activate(&ghost).await.unwrap_err().to_string();
        assert!(
            err.contains("account not found") && err.contains("ghost@example.com"),
            "unknown accounts must still resolve first, got: {err}"
        );
    }

    /// 过期 parked 回归：access 已过期且有 refresh（macOS 路径会走 pre-refresh），
    /// 非 macOS 仍必须在任何刷新/网络/capture 之前返回不支持错误；blob 与 registry 原样。
    /// 超时守卫：若实现回退到走网络刷新，测试快速失败而非挂起。
    #[tokio::test]
    async fn unsupported_activation_never_refreshes_expired_parked() {
        let tmp = tempfile::tempdir().unwrap();
        let (provider, id, raw) =
            fixture_with_expiry(&tmp, chrono::Utc::now() - chrono::Duration::hours(1));
        let res = tokio::time::timeout(std::time::Duration::from_secs(10), provider.activate(&id))
            .await
            .expect("unsupported activate must return promptly without network");
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("only supported on macOS"),
            "expired parked must still hit the explicit unsupported error, got: {err}"
        );
        // store 不动：过期 blob 原样（未被刷新轮换）。
        let kept_raw = provider
            .store
            .get(PROVIDER_ID, &id.0, STORE_FIELD)
            .unwrap()
            .unwrap();
        assert_eq!(
            kept_raw, raw,
            "unsupported swap must not rotate expired token"
        );
        // registry 不动：仍在且保持 parked。
        let kept = provider.registry.find(PROVIDER_ID, &id).unwrap().unwrap();
        assert!(!kept.active, "unsupported swap must not flip active");
        assert_eq!(kept.label, "fixture@example.com");
        // 客户端不动：桩 keychain 始终无 live。
        assert!(crate::keychain::find_password().unwrap().is_none());
    }
}

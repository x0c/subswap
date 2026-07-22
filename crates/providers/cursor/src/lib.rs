//! Cursor Provider：从 Cursor 的 `state.vscdb` 导入与切换账号，并查询官方用量。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs::OpenOptions, io::Write};

use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subswap_core::error::{Error, Result};
use subswap_core::swap::{persist_pre_swap_snapshot_in, SnapshotEntry};
use subswap_core::{
    Account, AccountId, AccountRegistry, ClientTarget, CredentialStore, Provider, Quota,
    QuotaStatus, QuotaWindow,
};

pub const PROVIDER_ID: &str = "cursor";
const STORE_FIELD: &str = "blob";
const USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const TOKEN_URL: &str = "https://api2.cursor.sh/oauth/token";
const CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";

const ACCESS_KEY: &str = "cursorAuth/accessToken";
const REFRESH_KEY: &str = "cursorAuth/refreshToken";
const EMAIL_KEY: &str = "cursorAuth/cachedEmail";
const AUTH_ID_KEY: &str = "cursorAuth/authId";
const MEMBERSHIP_KEY: &str = "cursorAuth/stripeMembershipType";
const SUBSCRIPTION_STATUS_KEY: &str = "cursorAuth/stripeSubscriptionStatus";
const SIGN_UP_TYPE_KEY: &str = "cursorAuth/cachedSignUpType";
const COMPAT_ACCESS_KEY: &str = "cursor.accessToken";
const COMPAT_EMAIL_KEY: &str = "cursor.email";
const SWAP_KEYS: [&str; 9] = [
    ACCESS_KEY,
    REFRESH_KEY,
    EMAIL_KEY,
    AUTH_ID_KEY,
    MEMBERSHIP_KEY,
    SUBSCRIPTION_STATUS_KEY,
    SIGN_UP_TYPE_KEY,
    COMPAT_ACCESS_KEY,
    COMPAT_EMAIL_KEY,
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CursorBlob {
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    membership_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subscription_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sign_up_type: Option<String>,
}

#[derive(Serialize)]
struct CursorStateSnapshot {
    state_db: String,
    values: std::collections::BTreeMap<&'static str, Option<String>>,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    #[serde(alias = "accessToken")]
    access_token: Option<String>,
    #[serde(alias = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(default, alias = "shouldLogout")]
    should_logout: bool,
}

#[derive(Clone)]
pub struct CursorProvider {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    state_db: PathBuf,
    usage_url: String,
    token_url: String,
    client: reqwest::Client,
    process_control: Arc<dyn CursorProcessControl>,
    refresh_lock_dir: PathBuf,
    snapshots_dir: PathBuf,
}

struct CursorProviderConfig {
    state_db: PathBuf,
    usage_url: String,
    token_url: String,
    process_control: Arc<dyn CursorProcessControl>,
    refresh_lock_dir: PathBuf,
    snapshots_dir: PathBuf,
}

impl CursorProvider {
    pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> Self {
        let paths = subswap_core::paths::AppPaths::resolve().ok();
        let refresh_lock_dir = paths
            .as_ref()
            .map(|paths| paths.state_dir.join("cursor-refresh"))
            .unwrap_or_else(|| std::env::temp_dir().join("subswap-cursor-refresh"));
        let snapshots_dir = paths
            .map(|paths| paths.snapshots_dir())
            .unwrap_or_else(|| std::env::temp_dir().join("subswap-snapshots"));
        Self::with_config(
            store,
            registry,
            CursorProviderConfig {
                state_db: default_state_db_path(),
                usage_url: USAGE_URL.to_string(),
                token_url: TOKEN_URL.to_string(),
                process_control: Arc::new(SystemCursorProcessControl),
                refresh_lock_dir,
                snapshots_dir,
            },
        )
    }

    fn with_config(
        store: Arc<dyn CredentialStore>,
        registry: Arc<AccountRegistry>,
        config: CursorProviderConfig,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("Cursor HTTP client configuration must be valid");
        Self {
            store,
            registry,
            state_db: config.state_db,
            usage_url: config.usage_url,
            token_url: config.token_url,
            client,
            process_control: config.process_control,
            refresh_lock_dir: config.refresh_lock_dir,
            snapshots_dir: config.snapshots_dir,
        }
    }

    /// 导入 Cursor 当前登录账号，并将它标记为 active。
    pub async fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.import_active_blocking(label_hint))
            .await
            .map_err(join_error)?
    }

    /// 对齐当前 Cursor 登录账号的元数据。Cursor 凭证位于普通 SQLite 文件，无钥匙串弹窗。
    pub async fn sync_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        self.import_active(label_hint).await
    }

    /// 把当前 Cursor 登录凭证回灌到其账号副本，供 daemon 捕获客户端自行轮换的 token。
    pub async fn reconcile_active_from_live(&self) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let live = read_live_blob(&this.state_db)?;
            let owner = this.find_owner(&live)?;
            this.capture_live_into_store(&live)?;
            // 只对已导入账号对齐 active；未知账号不由 daemon 擅自新增。
            if let Some(owner) = owner {
                this.registry.set_active(PROVIDER_ID, &owner.id)?;
            }
            Ok(())
        })
        .await
        .map_err(join_error)?
    }

    fn import_active_blocking(&self, label_hint: Option<String>) -> Result<Account> {
        let blob = read_live_blob(&self.state_db)?;
        self.upsert_blob(blob, label_hint, true)
    }

    fn upsert_blob(
        &self,
        blob: CursorBlob,
        label_hint: Option<String>,
        active: bool,
    ) -> Result<Account> {
        validate_blob(&blob)?;
        let existing = self.find_owner(&blob)?;
        let id = existing
            .as_ref()
            .map(|account| account.id.clone())
            .unwrap_or_else(|| AccountId(identity_for(&blob)));
        let raw = serde_json::to_string(&blob)?;
        self.store.set(PROVIDER_ID, &id.0, STORE_FIELD, &raw)?;

        let mut extra = existing
            .as_ref()
            .map(|account| account.extra.clone())
            .unwrap_or_default();
        extra.insert("email".into(), Value::String(blob.email.clone()));
        if let Some(auth_id) = &blob.auth_id {
            extra.insert("auth_id".into(), Value::String(auth_id.clone()));
        }
        let account = Account {
            provider: PROVIDER_ID.into(),
            id,
            label: label_hint
                .filter(|label| !label.trim().is_empty())
                .or_else(|| existing.as_ref().map(|account| account.label.clone()))
                .unwrap_or_else(|| blob.email.clone()),
            active,
            created_at: existing
                .as_ref()
                .map(|account| account.created_at)
                .unwrap_or_else(Utc::now),
            last_used_at: existing.and_then(|account| account.last_used_at),
            priority: 100,
            extra,
        };
        self.registry.upsert(account.clone())?;
        if active {
            self.registry.set_active(PROVIDER_ID, &account.id)?;
        }
        Ok(self
            .registry
            .find(PROVIDER_ID, &account.id)?
            .unwrap_or(account))
    }

    fn find_owner(&self, blob: &CursorBlob) -> Result<Option<Account>> {
        let accounts = self.registry.list_by_provider(PROVIDER_ID)?;
        Ok(accounts
            .into_iter()
            .find(|account| account_matches_blob(account, blob)))
    }

    fn require_account(&self, id: &AccountId) -> Result<Account> {
        self.registry
            .find(PROVIDER_ID, id)?
            .ok_or_else(|| Error::AccountNotFound {
                provider: PROVIDER_ID.into(),
                id: id.to_string(),
            })
    }

    fn stored_blob(&self, account: &Account) -> Result<CursorBlob> {
        let raw = self
            .store
            .get(PROVIDER_ID, &account.id.0, STORE_FIELD)?
            .ok_or_else(|| {
                Error::Credential(format!("no stored credentials for cursor:{}", account.id))
            })?;
        parse_blob(&raw)
    }

    fn capture_live_into_store(&self, live: &CursorBlob) -> Result<()> {
        let Some(owner) = self.find_owner(live)? else {
            return Ok(());
        };
        if live.refresh_token.is_none()
            && self
                .stored_blob(&owner)
                .ok()
                .is_some_and(|stored| stored.refresh_token.is_some())
        {
            tracing::warn!(account = %owner.id, "skip Cursor live capture without refresh token");
            return Ok(());
        }
        self.store.set(
            PROVIDER_ID,
            &owner.id.0,
            STORE_FIELD,
            &serde_json::to_string(live)?,
        )
    }

    fn activate_blocking(&self, id: &AccountId) -> Result<()> {
        let _switch_lock = self.acquire_switch_lock()?;
        let account = self.require_account(id)?;

        let cursor_was_running = self.process_control.is_running()?;
        if cursor_was_running {
            // 必须先等 Cursor 完全退出，再读写数据库；否则 Electron 退出时可能把
            // 内存中的旧凭证刷回 state.vscdb，覆盖刚完成的切换。
            self.process_control.stop()?;
        }
        let registry_before = match self.registry.load() {
            Ok(accounts) => accounts,
            Err(error) => return Err(self.restart_old_after_failure(cursor_was_running, error)),
        };
        let mut conn = match Connection::open(&self.state_db)
            .map_err(sql_error("open Cursor state database"))
        {
            Ok(conn) => conn,
            Err(error) => return Err(self.restart_old_after_failure(cursor_was_running, error)),
        };
        let live = match read_blob_from_connection(&conn) {
            Ok(live) => live,
            Err(error) => return Err(self.restart_old_after_failure(cursor_was_running, error)),
        };
        if let Err(error) = self.capture_live_into_store(&live) {
            return Err(self.restart_old_after_failure(cursor_was_running, error));
        }
        // stop 后的 capture 可能刚更新目标账号（例如重复激活当前账号），此处再取一次，
        // 避免把客户端刚轮换的新 token 又覆盖成旧副本。
        let target = match self.stored_blob(&account) {
            Ok(target) => target,
            Err(error) => return Err(self.restart_old_after_failure(cursor_was_running, error)),
        };
        if let Err(error) = validate_blob(&target) {
            return Err(self.restart_old_after_failure(cursor_was_running, error));
        }
        let before = match snapshot_items(&conn) {
            Ok(before) => before,
            Err(error) => return Err(self.restart_old_after_failure(cursor_was_running, error)),
        };
        if let Err(error) = self.persist_pre_swap_snapshot(&before) {
            return Err(self.restart_old_after_failure(cursor_was_running, error));
        }
        {
            let write_result = (|| {
                let tx = conn
                    .transaction()
                    .map_err(sql_error("begin Cursor credential transaction"))?;
                write_blob_to_transaction(&tx, &target)?;
                tx.commit()
                    .map_err(sql_error("commit Cursor credential transaction"))
            })();
            if let Err(error) = write_result {
                return Err(self.restart_old_after_failure(cursor_was_running, error));
            }
        }
        if let Err(error) = self.registry.set_active(PROVIDER_ID, id) {
            let db_rollback = restore_items(&mut conn, &before);
            let registry_rollback = self.registry.save(&registry_before);
            return match (db_rollback, registry_rollback) {
                (Ok(()), Ok(())) => {
                    Err(self.restart_old_after_failure(cursor_was_running, error))
                }
                (db, registry) => Err(Error::Provider(format!(
                    "mark Cursor account active failed: {error}; database rollback: {}; registry rollback: {}",
                    rollback_result(db),
                    rollback_result(registry)
                ))),
            };
        }
        if cursor_was_running {
            if let Err(start_error) = self.process_control.start() {
                let db_rollback = restore_items(&mut conn, &before);
                let registry_rollback = self.registry.save(&registry_before);
                if let Err(error) = db_rollback {
                    return Err(Error::Provider(format!(
                        "start Cursor failed: {start_error}; database rollback failed: {error}"
                    )));
                }
                if let Err(error) = registry_rollback {
                    return Err(Error::Provider(format!(
                        "start Cursor failed: {start_error}; registry rollback failed: {error}"
                    )));
                }
                // 两处状态都恢复旧值后，才重新启动原 Cursor 会话。
                let recovery_start = self.process_control.start();
                return Err(Error::Provider(match recovery_start {
                    Ok(()) => format!(
                        "start Cursor failed and the account switch was rolled back: {start_error}"
                    ),
                    Err(error) => format!(
                        "start Cursor failed and the account switch was rolled back; Cursor could not be reopened: {start_error}; {error}"
                    ),
                }));
            }
        }
        Ok(())
    }

    fn restart_old_after_failure(&self, cursor_was_running: bool, error: Error) -> Error {
        if !cursor_was_running {
            return error;
        }
        match self.process_control.start() {
            Ok(()) => error,
            Err(start_error) => Error::Provider(format!(
                "{error}; reopening the original Cursor session also failed: {start_error}"
            )),
        }
    }

    fn persist_pre_swap_snapshot(&self, state: &[(&'static str, Option<String>)]) -> Result<()> {
        let cursor_state = CursorStateSnapshot {
            state_db: self.state_db.display().to_string(),
            values: state.iter().cloned().collect(),
        };
        let registry = std::fs::read(self.registry.path())?;
        persist_pre_swap_snapshot_in(
            PROVIDER_ID,
            &self.snapshots_dir,
            vec![
                SnapshotEntry {
                    name: "cursor-state.json".into(),
                    content: serde_json::to_vec_pretty(&cursor_state)?,
                },
                SnapshotEntry {
                    name: "registry.toml".into(),
                    content: registry,
                },
            ],
        )?;
        Ok(())
    }

    async fn query_quota_inner(&self, id: AccountId) -> Result<Vec<Quota>> {
        let this = self.clone();
        let account = tokio::task::spawn_blocking(move || this.require_account(&id))
            .await
            .map_err(join_error)??;

        let (mut blob, source) = self.blob_for_query(account.clone()).await?;
        match self.fetch_usage(&account.id, &blob).await {
            Ok(quotas) => Ok(quotas),
            Err(UsageError::Unauthorized) if source == QuerySource::LiveOwner => {
                let this = self.clone();
                let account_again = account.clone();
                let fresh = tokio::task::spawn_blocking(move || {
                    let live = read_live_blob(&this.state_db)?;
                    if account_matches_blob(&account_again, &live) {
                        this.capture_live_into_store(&live)?;
                        Ok::<Option<CursorBlob>, Error>(Some(live))
                    } else {
                        Ok::<Option<CursorBlob>, Error>(None)
                    }
                })
                .await
                .map_err(join_error)??;
                if let Some(fresh) = fresh {
                    if fresh.access_token != blob.access_token {
                        return self
                            .fetch_usage(&account.id, &fresh)
                            .await
                            .map_err(usage_error);
                    }
                }
                Err(usage_error(UsageError::Unauthorized))
            }
            // registry active 可能因 DB 暂时不可读或原生换号而漂移；active 账号
            // 永远不以 parked 身份刷新，避免与 Cursor 争用一次性 refresh token。
            Err(UsageError::Unauthorized) if account.active => {
                Err(usage_error(UsageError::Unauthorized))
            }
            Err(UsageError::Unauthorized) if source == QuerySource::ParkedConfirmed => {
                blob = self.refresh_parked(&account, blob).await?;
                self.fetch_usage(&account.id, &blob)
                    .await
                    .map_err(usage_error)
            }
            Err(UsageError::Unauthorized) => Err(usage_error(UsageError::Unauthorized)),
            Err(error) => Err(usage_error(error)),
        }
    }

    async fn blob_for_query(&self, account: Account) -> Result<(CursorBlob, QuerySource)> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || match read_live_blob(&this.state_db) {
            Ok(live) if account_matches_blob(&account, &live) => {
                this.capture_live_into_store(&live)?;
                Ok((live, QuerySource::LiveOwner))
            }
            Ok(_) => Ok((this.stored_blob(&account)?, QuerySource::ParkedConfirmed)),
            Err(_) => Ok((this.stored_blob(&account)?, QuerySource::LiveUnreadable)),
        })
        .await
        .map_err(join_error)?
    }

    async fn fetch_usage(
        &self,
        id: &AccountId,
        blob: &CursorBlob,
    ) -> std::result::Result<Vec<Quota>, UsageError> {
        let cookie = session_cookie(&blob.access_token).ok_or_else(|| {
            UsageError::Other("access token does not contain a WorkOS user ID".into())
        })?;
        let response = self
            .client
            .get(&self.usage_url)
            .header("Accept", "application/json")
            .header("Cookie", cookie)
            .header("User-Agent", "Mozilla/5.0 (subswap Cursor quota)")
            .send()
            .await
            .map_err(|error| UsageError::Other(format!("request failed: {error}")))?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(UsageError::Unauthorized);
        }
        if !response.status().is_success() {
            return Err(UsageError::Other(format!(
                "usage API returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|error| UsageError::Other(format!("invalid usage response: {error}")))?;
        parse_usage(id, &body)
    }

    async fn refresh_parked(
        &self,
        account: &Account,
        original_blob: CursorBlob,
    ) -> Result<CursorBlob> {
        let this = self.clone();
        let account_id = account.id.clone();
        let original_access = original_blob.access_token;
        let (guard, mut blob, dead_fingerprint) = tokio::task::spawn_blocking(move || {
            let guard = this.acquire_refresh_lock(&account_id)?;
            // 锁内重读：另一进程可能已经完成一次性 refresh token 轮换。
            let latest = this.stored_blob(&this.require_account(&account_id)?)?;
            let dead_fingerprint = guard.dead_fingerprint()?;
            Ok::<_, Error>((guard, latest, dead_fingerprint))
        })
        .await
        .map_err(join_error)??;
        if blob.access_token != original_access {
            return Ok(blob);
        }
        let refresh_token = blob.refresh_token.clone().ok_or_else(|| {
            Error::QuotaFetch(
                "Cursor session expired and no refresh token is stored; run `subswap login cursor`"
                    .into(),
            )
        })?;
        let refresh_fingerprint = sha256_hex(refresh_token.as_bytes());
        if dead_fingerprint.as_deref() == Some(refresh_fingerprint.as_str()) {
            return Err(Error::QuotaFetch(
                "Cursor refresh token is invalid; run `subswap login cursor`".into(),
            ));
        }
        let response = self
            .client
            .post(&self.token_url)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": refresh_token,
            }))
            .send()
            .await
            .map_err(|error| Error::QuotaFetch(format!("Cursor token refresh failed: {error}")))?;
        if matches!(response.status().as_u16(), 401 | 403) {
            tokio::task::spawn_blocking(move || guard.mark_dead(&refresh_fingerprint))
                .await
                .map_err(join_error)??;
            return Err(Error::QuotaFetch(
                "Cursor refresh token is invalid; run `subswap login cursor`".into(),
            ));
        }
        if !response.status().is_success() {
            return Err(Error::QuotaFetch(format!(
                "Cursor token refresh returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let refreshed: RefreshResponse = response.json().await.map_err(|error| {
            Error::QuotaFetch(format!("invalid Cursor token response: {error}"))
        })?;
        if refreshed.should_logout {
            tokio::task::spawn_blocking(move || guard.mark_dead(&refresh_fingerprint))
                .await
                .map_err(join_error)??;
            return Err(Error::QuotaFetch(
                "Cursor refresh token is invalid; run `subswap login cursor`".into(),
            ));
        }
        blob.access_token = non_empty(refreshed.access_token).ok_or_else(|| {
            Error::QuotaFetch("Cursor token response is missing access_token".into())
        })?;
        if let Some(rotated) = non_empty(refreshed.refresh_token) {
            blob.refresh_token = Some(rotated);
        }
        let raw = serde_json::to_string(&blob)?;
        let store = self.store.clone();
        let account_id = account.id.0.clone();
        tokio::task::spawn_blocking(move || store.set(PROVIDER_ID, &account_id, STORE_FIELD, &raw))
            .await
            .map_err(join_error)??;
        tokio::task::spawn_blocking(move || guard.clear_dead())
            .await
            .map_err(join_error)??;
        Ok(blob)
    }

    fn acquire_refresh_lock(&self, id: &AccountId) -> Result<RefreshLock> {
        std::fs::create_dir_all(&self.refresh_lock_dir)?;
        let name = sha256_hex(id.0.as_bytes());
        let lock_path = self.refresh_lock_dir.join(format!("{name}.lock"));
        let dead_path = self.refresh_lock_dir.join(format!("{name}.dead"));
        let file = acquire_bounded_lock(
            &lock_path,
            "timed out waiting for another Cursor token refresh",
        )?;
        Ok(RefreshLock { file, dead_path })
    }

    fn acquire_switch_lock(&self) -> Result<SwitchLock> {
        std::fs::create_dir_all(&self.refresh_lock_dir)?;
        let path = self.refresh_lock_dir.join("cursor-switch.lock");
        let file =
            acquire_bounded_lock(&path, "timed out waiting for another Cursor account switch")?;
        Ok(SwitchLock { file })
    }
}

#[async_trait]
impl Provider for CursorProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "Cursor"
    }

    fn client_targets(&self) -> Vec<ClientTarget> {
        vec![ClientTarget {
            id: "cursor_desktop".into(),
            display_name: "Cursor desktop credentials".into(),
            probe_path: self.state_db.clone(),
        }]
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        let registry = self.registry.clone();
        tokio::task::spawn_blocking(move || registry.list_by_provider(PROVIDER_ID))
            .await
            .map_err(join_error)?
    }

    async fn activate(&self, id: &AccountId) -> Result<()> {
        let this = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || this.activate_blocking(&id))
            .await
            .map_err(join_error)?
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        self.query_quota_inner(id.clone()).await
    }
}

fn default_state_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SUBSWAP_CURSOR_STATE_DB_PATH") {
        return PathBuf::from(path);
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = directories::BaseDirs::new() {
        return home
            .home_dir()
            .join("Library/Application Support/Cursor/User/globalStorage/state.vscdb");
    }
    #[cfg(target_os = "windows")]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("Cursor/User/globalStorage/state.vscdb");
    }
    #[cfg(target_os = "linux")]
    if let Some(home) = directories::BaseDirs::new() {
        return home
            .home_dir()
            .join(".config/Cursor/User/globalStorage/state.vscdb");
    }
    PathBuf::from("state.vscdb")
}

fn read_live_blob(path: &Path) -> Result<CursorBlob> {
    if !path.exists() {
        return Err(Error::Provider(format!(
            "Cursor state database not found at {}; sign in to Cursor first",
            path.display()
        )));
    }
    let conn = Connection::open(path).map_err(sql_error("open Cursor state database"))?;
    read_blob_from_connection(&conn)
}

fn read_blob_from_connection(conn: &Connection) -> Result<CursorBlob> {
    let access_token = read_item(conn, ACCESS_KEY)?.ok_or_else(|| {
        Error::Provider("Cursor is not signed in; sign in to Cursor first".into())
    })?;
    let email = read_item(conn, EMAIL_KEY)?
        .or_else(|| read_item(conn, COMPAT_EMAIL_KEY).ok().flatten())
        .ok_or_else(|| Error::Provider("Cursor credentials are missing cachedEmail".into()))?;
    let blob = CursorBlob {
        auth_id: read_item(conn, AUTH_ID_KEY)?.or_else(|| jwt_subject(&access_token)),
        refresh_token: read_item(conn, REFRESH_KEY)?,
        access_token,
        email,
        membership_type: read_item(conn, MEMBERSHIP_KEY)?,
        subscription_status: read_item(conn, SUBSCRIPTION_STATUS_KEY)?,
        sign_up_type: read_item(conn, SIGN_UP_TYPE_KEY)?,
    };
    validate_blob(&blob)?;
    Ok(blob)
}

fn read_item(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .optional()
    .map(|value: Option<String>| value.and_then(|value| non_empty(Some(value))))
    .map_err(sql_error("read Cursor credential"))
}

fn snapshot_items(conn: &Connection) -> Result<Vec<(&'static str, Option<String>)>> {
    SWAP_KEYS
        .iter()
        .map(|key| read_item(conn, key).map(|value| (*key, value)))
        .collect()
}

fn write_blob_to_transaction(tx: &Transaction<'_>, blob: &CursorBlob) -> Result<()> {
    upsert_item(tx, ACCESS_KEY, &blob.access_token)?;
    set_optional_item(tx, REFRESH_KEY, blob.refresh_token.as_deref())?;
    upsert_item(tx, EMAIL_KEY, &blob.email)?;
    set_optional_item(tx, AUTH_ID_KEY, blob.auth_id.as_deref())?;
    set_optional_item(tx, MEMBERSHIP_KEY, blob.membership_type.as_deref())?;
    set_optional_item(
        tx,
        SUBSCRIPTION_STATUS_KEY,
        blob.subscription_status.as_deref(),
    )?;
    set_optional_item(tx, SIGN_UP_TYPE_KEY, blob.sign_up_type.as_deref())?;
    upsert_item(tx, COMPAT_ACCESS_KEY, &blob.access_token)?;
    upsert_item(tx, COMPAT_EMAIL_KEY, &blob.email)
}

fn restore_items(conn: &mut Connection, items: &[(&'static str, Option<String>)]) -> Result<()> {
    let tx = conn
        .transaction()
        .map_err(sql_error("begin Cursor rollback transaction"))?;
    for (key, value) in items {
        set_optional_item(&tx, key, value.as_deref())?;
    }
    tx.commit()
        .map_err(sql_error("commit Cursor rollback transaction"))
}

fn upsert_item(tx: &Transaction<'_>, key: &str, value: &str) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
        (key, value),
    )
    .map(|_| ())
    .map_err(sql_error("write Cursor credential"))
}

fn set_optional_item(tx: &Transaction<'_>, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => upsert_item(tx, key, value),
        None => tx
            .execute("DELETE FROM ItemTable WHERE key = ?1", [key])
            .map(|_| ())
            .map_err(sql_error("clear Cursor credential")),
    }
}

fn parse_usage(id: &AccountId, root: &Value) -> std::result::Result<Vec<Quota>, UsageError> {
    let plan = root
        .pointer("/individualUsage/plan")
        .or_else(|| root.pointer("/individual_usage/plan"))
        .or_else(|| root.get("planUsage"))
        .or_else(|| root.get("plan_usage"))
        .ok_or_else(|| {
            UsageError::Other("usage response is missing individualUsage.plan".into())
        })?;
    let reset_at = root
        .get("billingCycleEnd")
        .or_else(|| root.get("billing_cycle_end"))
        .and_then(parse_reset_at);
    let mut quotas = Vec::new();
    if let Some(used) = pick_number(plan, &["autoPercentUsed", "auto_percent_used"]) {
        quotas.push(percent_quota(
            id,
            QuotaWindow::FirstPartyModels,
            used,
            reset_at,
        ));
    }
    if let Some(used) = pick_number(plan, &["apiPercentUsed", "api_percent_used"]) {
        quotas.push(percent_quota(id, QuotaWindow::Api, used, reset_at));
    }
    if quotas.is_empty() {
        return Err(UsageError::Other(
            "usage response contains neither autoPercentUsed nor apiPercentUsed".into(),
        ));
    }
    Ok(quotas)
}

fn percent_quota(
    id: &AccountId,
    window: QuotaWindow,
    value: f64,
    reset_at: Option<DateTime<Utc>>,
) -> Quota {
    let used = value.clamp(0.0, 100.0).round() as u64;
    Quota {
        provider: PROVIDER_ID.into(),
        account_id: id.clone(),
        window,
        used,
        limit: 100,
        reset_at,
        status: QuotaStatus::from_percent(used as f64),
        note: None,
    }
}

fn pick_number(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        value.get(key).and_then(|raw| {
            raw.as_f64()
                .or_else(|| raw.as_str().and_then(|text| text.trim().parse().ok()))
                .filter(|number| number.is_finite())
        })
    })
}

fn parse_reset_at(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(text) = value.as_str() {
        return DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|value| value.with_timezone(&Utc));
    }
    let seconds = value.as_i64()?;
    DateTime::from_timestamp(
        if seconds > 10_000_000_000 {
            seconds / 1000
        } else {
            seconds
        },
        0,
    )
}

fn account_matches_blob(account: &Account, blob: &CursorBlob) -> bool {
    if let Some(auth_id) = &blob.auth_id {
        if account.extra.get("auth_id").and_then(Value::as_str) == Some(auth_id) {
            return true;
        }
    }
    account
        .extra
        .get("email")
        .and_then(Value::as_str)
        .is_some_and(|email| email.eq_ignore_ascii_case(&blob.email))
        || account.id.0.eq_ignore_ascii_case(&blob.email)
}

fn identity_for(blob: &CursorBlob) -> String {
    blob.auth_id
        .clone()
        .unwrap_or_else(|| blob.email.to_lowercase())
}

fn validate_blob(blob: &CursorBlob) -> Result<()> {
    if blob.access_token.trim().is_empty() || blob.email.trim().is_empty() {
        return Err(Error::Provider(
            "Cursor credentials are missing accessToken or cachedEmail".into(),
        ));
    }
    Ok(())
}

fn parse_blob(raw: &str) -> Result<CursorBlob> {
    let blob: CursorBlob = serde_json::from_str(raw)?;
    validate_blob(&blob)?;
    Ok(blob)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

fn jwt_subject(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("sub")?
        .as_str()
        .map(str::to_string)
}

fn session_cookie(token: &str) -> Option<String> {
    let subject = jwt_subject(token)?;
    let user_id = subject.rsplit('|').next().unwrap_or(&subject);
    user_id
        .starts_with("user_")
        .then(|| format!("WorkosCursorSessionToken={user_id}%3A%3A{token}"))
}

fn sql_error(context: &'static str) -> impl FnOnce(rusqlite::Error) -> Error {
    move |error| Error::Provider(format!("{context}: {error}"))
}

fn join_error(error: tokio::task::JoinError) -> Error {
    Error::Provider(format!("Cursor blocking task failed: {error}"))
}

fn rollback_result(result: Result<()>) -> String {
    match result {
        Ok(()) => "ok".into(),
        Err(error) => error.to_string(),
    }
}

struct RefreshLock {
    file: std::fs::File,
    dead_path: PathBuf,
}

struct SwitchLock {
    file: std::fs::File,
}

impl Drop for SwitchLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl RefreshLock {
    fn dead_fingerprint(&self) -> Result<Option<String>> {
        match std::fs::read_to_string(&self.dead_path) {
            Ok(value) => Ok(non_empty(Some(value))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn mark_dead(&self, fingerprint: &str) -> Result<()> {
        let mut file = open_private_file(&self.dead_path)?;
        file.set_len(0)?;
        file.write_all(fingerprint.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    fn clear_dead(&self) -> Result<()> {
        match std::fs::remove_file(&self.dead_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn open_private_file(path: &Path) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn acquire_bounded_lock(path: &Path, timeout_message: &str) -> Result<std::fs::File> {
    let file = open_private_file(path)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(Error::Provider(timeout_message.into()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[derive(Debug)]
enum UsageError {
    Unauthorized,
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuerySource {
    LiveOwner,
    ParkedConfirmed,
    LiveUnreadable,
}

fn usage_error(error: UsageError) -> Error {
    match error {
        UsageError::Unauthorized => Error::QuotaFetch(
            "Cursor session is unauthorized; reopen Cursor or run `subswap login cursor`".into(),
        ),
        UsageError::Other(message) => Error::QuotaFetch(format!("Cursor {message}")),
    }
}

trait CursorProcessControl: Send + Sync {
    fn is_running(&self) -> Result<bool>;
    fn stop(&self) -> Result<()>;
    fn start(&self) -> Result<()>;
}

struct SystemCursorProcessControl;

impl CursorProcessControl for SystemCursorProcessControl {
    fn is_running(&self) -> Result<bool> {
        #[cfg(target_os = "windows")]
        let output = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq Cursor.exe", "/FO", "CSV", "/NH"])
            .output();
        #[cfg(target_os = "macos")]
        let output = Command::new("pgrep").args(["-x", "Cursor"]).output();
        #[cfg(all(unix, not(target_os = "macos")))]
        let output = Command::new("pgrep").args(["-x", "cursor"]).output();

        let output = output.map_err(|error| {
            Error::Provider(format!("detect running Cursor process failed: {error}"))
        })?;
        #[cfg(target_os = "windows")]
        return Ok(output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("Cursor.exe"));
        #[cfg(not(target_os = "windows"))]
        Ok(output.status.success() && !output.stdout.is_empty())
    }

    fn stop(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        let status = Command::new("osascript")
            .args(["-e", "tell application \"Cursor\" to quit"])
            .status();
        #[cfg(target_os = "windows")]
        let status = Command::new("taskkill")
            .args(["/IM", "Cursor.exe"])
            .status();
        #[cfg(all(unix, not(target_os = "macos")))]
        let status = Command::new("pkill")
            .args(["-TERM", "-x", "cursor"])
            .status();

        let status =
            status.map_err(|error| Error::Provider(format!("close Cursor failed: {error}")))?;
        if !status.success() {
            return Err(Error::Provider(format!(
                "close Cursor failed with status {status}"
            )));
        }
        for _ in 0..50 {
            if !self.is_running()? {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err(Error::Provider(
            "Cursor did not exit within 5 seconds; account switch was not attempted".into(),
        ))
    }

    fn start(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        let child = Command::new("open")
            .args(["-a", "Cursor"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        #[cfg(target_os = "windows")]
        let child = {
            let installed = std::env::var_os("LOCALAPPDATA").and_then(|value| {
                let root = PathBuf::from(value).join("Programs");
                ["cursor", "Cursor"]
                    .into_iter()
                    .map(|dir| root.join(dir).join("Cursor.exe"))
                    .find(|path| path.exists())
            });
            if let Some(executable) = installed {
                Command::new(executable)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
            } else {
                // 最后才交给 Windows App Paths / shell 解析，避免默认假定 Cursor 在 PATH。
                Command::new("cmd")
                    .args(["/C", "start", "", "Cursor"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
            }
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        let child = Command::new("cursor")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        child.map_err(|error| Error::Provider(format!("start Cursor failed: {error}")))?;
        for _ in 0..100 {
            if self.is_running()? {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err(Error::Provider(
            "Cursor did not start within 10 seconds".into(),
        ))
    }
}

#[cfg(test)]
mod tests;

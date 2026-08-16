//! Cursor Provider：导入与切换 Cursor 账号并查询官方用量。
//! 支持两种凭证来源：桌面版 Electron 的 `state.vscdb`（SQLite），以及命令行
//! agent（cursor-agent）。agent 的 token 在 Linux 默认是 `~/.config/cursor/auth.json`，
//! 在 macOS 默认是系统钥匙串（`cursor-access-token` / `cursor-refresh-token`），
//! 文件后端时落在 `~/.cursor/auth.json`；邮箱等元数据在 `~/.cursor/cli-config.json`
//! 的 `authInfo`。两种来源共用同一套额度查询与 refresh token 轮换逻辑。

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

/// 桌面版 state.vscdb 路径覆盖（绝对路径），主要给测试与非标准安装用。
const STATE_DB_ENV: &str = "SUBSWAP_CURSOR_STATE_DB_PATH";
/// 命令行 agent 的 auth.json 路径覆盖（绝对路径）。
const AGENT_AUTH_ENV: &str = "SUBSWAP_CURSOR_AGENT_AUTH_PATH";
/// 命令行 agent 的 cli-config.json 路径覆盖（绝对路径）。
const AGENT_CONFIG_ENV: &str = "SUBSWAP_CURSOR_AGENT_CONFIG_PATH";
/// macOS 命令行 agent 钥匙串文件覆盖（绝对路径）。测试必须设置，禁止碰真实登录钥匙串。
#[cfg(target_os = "macos")]
const AGENT_KEYCHAIN_ENV: &str = "SUBSWAP_CURSOR_KEYCHAIN_PATH";
#[cfg(target_os = "macos")]
const AGENT_ACCESS_SERVICE: &str = "cursor-access-token";
#[cfg(target_os = "macos")]
const AGENT_REFRESH_SERVICE: &str = "cursor-refresh-token";
#[cfg(target_os = "macos")]
const AGENT_KEYCHAIN_ACCOUNT: &str = "cursor-user";

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

/// 命令行 agent 的 token 后端。邮箱等元数据始终在 `cli-config.json`。
#[derive(Clone, Debug)]
enum AgentTokenStore {
    /// `auth.json` 文件（Linux 默认；macOS 仅在官方文件后端时使用）。
    File,
    /// macOS 钥匙串（cursor-agent 在 Darwin 上的默认后端）。
    #[cfg(target_os = "macos")]
    Keychain { path: Option<PathBuf> },
}

/// Cursor 凭证来源。桌面版与命令行 agent 的存储布局不同，但对上层是同一套账号语义。
#[derive(Clone, Debug)]
enum CredentialSource {
    /// 桌面版 Electron：凭证在 `state.vscdb`（SQLite ItemTable）。
    Desktop { state_db: PathBuf },
    /// 命令行 agent：token 在文件或 macOS 钥匙串，邮箱等元数据在 `cli-config.json`。
    Agent {
        auth_json: PathBuf,
        cli_config: PathBuf,
        token_store: AgentTokenStore,
    },
}

impl CredentialSource {
    /// 读取当前登录的活跃凭证。
    fn read_live(&self) -> Result<CursorBlob> {
        match self {
            CredentialSource::Desktop { state_db } => read_live_blob(state_db),
            CredentialSource::Agent {
                auth_json,
                cli_config,
                token_store,
            } => read_agent_blob(auth_json, cli_config, token_store),
        }
    }

    /// 对外暴露的凭证探针路径（用于 client target 与快照标签）。
    fn probe_path(&self) -> PathBuf {
        match self {
            CredentialSource::Desktop { state_db } => state_db.clone(),
            CredentialSource::Agent { auth_json, .. } => auth_json.clone(),
        }
    }
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
    source: CredentialSource,
    usage_url: String,
    token_url: String,
    client: reqwest::Client,
    process_control: Arc<dyn CursorProcessControl>,
    refresh_lock_dir: PathBuf,
    snapshots_dir: PathBuf,
}

struct CursorProviderConfig {
    source: CredentialSource,
    usage_url: String,
    token_url: String,
    process_control: Arc<dyn CursorProcessControl>,
    refresh_lock_dir: PathBuf,
    snapshots_dir: PathBuf,
}

impl CursorProvider {
    pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> Result<Self> {
        let paths = subswap_core::paths::AppPaths::resolve().ok();
        let refresh_lock_dir = paths
            .as_ref()
            .map(|paths| paths.state_dir.join("cursor-refresh"))
            .unwrap_or_else(|| std::env::temp_dir().join("subswap-cursor-refresh"));
        let snapshots_dir = paths
            .map(|paths| paths.snapshots_dir())
            .unwrap_or_else(|| std::env::temp_dir().join("subswap-snapshots"));
        Ok(Self::with_config(
            store,
            registry,
            CursorProviderConfig {
                source: default_credential_source()?,
                usage_url: USAGE_URL.to_string(),
                token_url: TOKEN_URL.to_string(),
                process_control: Arc::new(SystemCursorProcessControl),
                refresh_lock_dir,
                snapshots_dir,
            },
        ))
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
            source: config.source,
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

    /// 当前客户端登录账号的 registry id。`rm` 用它判断删除的号是否仍在客户端登录着，
    /// 默认入口用它判断「客户端登录着但同步失败」要不要提示。
    pub async fn live_account_id(&self) -> Result<AccountId> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.live_account_id_blocking())
            .await
            .map_err(join_error)?
    }

    fn live_account_id_blocking(&self) -> Result<AccountId> {
        let live = self.canonicalize_live_blob(self.source.read_live()?)?;
        if let Some(owner) = self.find_owner(&live)? {
            return Ok(owner.id);
        }
        Ok(AccountId(identity_for(&live)))
    }

    /// 对齐当前 Cursor 登录账号。已导入则回灌；列表里没有则像 Claude/Codex/Kimi 一样自动收入。
    /// 显式 `rm` 过的号只要客户端仍登录着，下次默认入口就会经这里重新收入——不再有墓碑拦截。
    pub async fn sync_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.sync_active_metadata_blocking(label_hint))
            .await
            .map_err(join_error)?
    }

    /// 把当前 Cursor 登录凭证回灌到其账号副本，供 daemon 捕获客户端自行轮换的 token。
    pub async fn reconcile_active_from_live(&self) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || match this.align_existing_live_blocking() {
            Ok(_) | Err(Error::AccountNotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        })
        .await
        .map_err(join_error)?
    }

    fn sync_active_metadata_blocking(&self, label_hint: Option<String>) -> Result<Account> {
        match self.align_existing_live_blocking() {
            Ok(account) => Ok(account),
            Err(Error::AccountNotFound { .. }) => self.import_active_blocking(label_hint),
            Err(error) => Err(error),
        }
    }

    /// 只回灌已导入的 live 主人；未知账号不新增（daemon 用）。
    fn align_existing_live_blocking(&self) -> Result<Account> {
        let live = self.canonicalize_live_blob(self.source.read_live()?)?;
        let Some(owner) = self.find_owner(&live)? else {
            return Err(Error::AccountNotFound {
                provider: PROVIDER_ID.into(),
                id: live.email,
            });
        };
        self.capture_live_into_store(&live)?;
        self.registry.set_active(PROVIDER_ID, &owner.id)?;
        self.registry
            .find(PROVIDER_ID, &owner.id)?
            .ok_or_else(|| Error::AccountNotFound {
                provider: PROVIDER_ID.into(),
                id: owner.id.to_string(),
            })
    }

    fn import_active_blocking(&self, label_hint: Option<String>) -> Result<Account> {
        let blob = self.canonicalize_live_blob(self.source.read_live()?)?;
        self.upsert_blob(blob, label_hint, true)
    }

    /// 令牌 JWT 才是 live 归属；过期的 cli-config 邮箱不得开出幽灵账号，也不得改写真正主人的身份字段。
    fn canonicalize_live_blob(&self, mut live: CursorBlob) -> Result<CursorBlob> {
        let Some(owner) = self.find_owner(&live)? else {
            return Ok(live);
        };
        if owner_email_matches(&owner, &live) {
            return Ok(live);
        }
        if let Ok(stored) = self.stored_blob(&owner) {
            live.email = stored.email;
            live.auth_id = stored.auth_id.or(live.auth_id);
            live.membership_type = stored.membership_type.or(live.membership_type);
            live.subscription_status = stored.subscription_status.or(live.subscription_status);
            live.sign_up_type = stored.sign_up_type.or(live.sign_up_type);
        } else if let Some(email) = owner.extra.get("email").and_then(Value::as_str) {
            live.email = email.to_string();
            live.auth_id = owner
                .extra
                .get("auth_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(live.auth_id);
        }
        Ok(live)
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
        let stored = self.stored_blob(&owner).ok();
        if live.refresh_token.is_none()
            && stored
                .as_ref()
                .is_some_and(|blob| blob.refresh_token.is_some())
        {
            tracing::warn!(account = %owner.id, "skip Cursor live capture without refresh token");
            return Ok(());
        }
        let mut to_store = live.clone();
        if let Some(stored) = stored {
            if !owner_email_matches(&owner, &to_store) {
                to_store.email = stored.email;
                to_store.auth_id = stored.auth_id.or(to_store.auth_id);
                to_store.membership_type = stored.membership_type.or(to_store.membership_type);
                to_store.subscription_status =
                    stored.subscription_status.or(to_store.subscription_status);
                to_store.sign_up_type = stored.sign_up_type.or(to_store.sign_up_type);
            }
        }
        self.store.set(
            PROVIDER_ID,
            &owner.id.0,
            STORE_FIELD,
            &serde_json::to_string(&to_store)?,
        )
    }

    fn activate_blocking(&self, id: &AccountId) -> Result<()> {
        match self.source.clone() {
            CredentialSource::Desktop { state_db } => self.activate_desktop_blocking(id, &state_db),
            CredentialSource::Agent {
                auth_json,
                cli_config,
                token_store,
            } => self.activate_agent_blocking(id, &auth_json, &cli_config, &token_store),
        }
    }

    /// 桌面版切换：停 Cursor → 写 SQLite → 标记 active，任一步失败按序回滚并重开原会话。
    fn activate_desktop_blocking(&self, id: &AccountId, state_db: &Path) -> Result<()> {
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
        let mut conn =
            match Connection::open(state_db).map_err(sql_error("open Cursor state database")) {
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
        if let Err(error) = reject_foreign_credentials(&account, &target) {
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

    /// 命令行 agent 切换：令牌与 cli-config 身份必须成套写入，失败则两边一起回滚。
    /// agent 无 GUI 进程生命周期需协调，写回后 cursor-agent 下次读取即生效。
    fn activate_agent_blocking(
        &self,
        id: &AccountId,
        auth_json: &Path,
        cli_config: &Path,
        token_store: &AgentTokenStore,
    ) -> Result<()> {
        let _switch_lock = self.acquire_switch_lock()?;
        let account = self.require_account(id)?;

        // 覆盖前先把当前 agent 登录凭证回灌其 JWT 主人，避免丢失客户端刚轮换的 token。
        if let Ok(live) = self.source.read_live() {
            self.capture_live_into_store(&live)?;
        }
        let target = self.stored_blob(&account)?;
        validate_blob(&target)?;
        reject_foreign_credentials(&account, &target)?;

        let previous_config = std::fs::read(cli_config).ok();
        let registry_before = self.registry.load()?;
        let previous_tokens = snapshot_agent_tokens(auth_json, token_store)?;
        self.persist_pre_swap_agent_bundle(&previous_tokens, previous_config.as_deref())?;

        if let Err(error) = write_agent_live(auth_json, cli_config, token_store, &target) {
            let token_rollback = restore_agent_tokens(auth_json, token_store, &previous_tokens);
            let config_rollback = restore_bytes(cli_config, previous_config.as_deref());
            return Err(Error::Provider(format!(
                "write Cursor CLI credentials failed: {error}; token rollback: {}; cli-config rollback: {}",
                rollback_result(token_rollback),
                rollback_result(config_rollback)
            )));
        }
        if let Err(error) = self.registry.set_active(PROVIDER_ID, id) {
            let token_rollback = restore_agent_tokens(auth_json, token_store, &previous_tokens);
            let config_rollback = restore_bytes(cli_config, previous_config.as_deref());
            let registry_rollback = self.registry.save(&registry_before);
            return Err(Error::Provider(format!(
                "mark Cursor account active failed: {error}; token rollback: {}; cli-config rollback: {}; registry rollback: {}",
                rollback_result(token_rollback),
                rollback_result(config_rollback),
                rollback_result(registry_rollback)
            )));
        }
        Ok(())
    }

    fn persist_pre_swap_agent_bundle(
        &self,
        previous_tokens: &AgentTokenSnapshot,
        previous_config: Option<&[u8]>,
    ) -> Result<()> {
        let registry = std::fs::read(self.registry.path())?;
        let mut entries = vec![
            SnapshotEntry {
                name: "registry.toml".into(),
                content: registry,
            },
            SnapshotEntry {
                name: "cursor-agent-tokens.json".into(),
                content: serde_json::to_vec_pretty(previous_tokens)?,
            },
        ];
        if let Some(previous_config) = previous_config {
            entries.push(SnapshotEntry {
                name: "cursor-cli-config.json".into(),
                content: previous_config.to_vec(),
            });
        }
        persist_pre_swap_snapshot_in(PROVIDER_ID, &self.snapshots_dir, entries)?;
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
            state_db: self.source.probe_path().display().to_string(),
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
        reject_foreign_credentials(&account, &blob)?;
        match self.fetch_usage(&account.id, &blob).await {
            Ok(quotas) => Ok(quotas),
            Err(UsageError::Unauthorized) if source == QuerySource::LiveOwner => {
                let this = self.clone();
                let account_again = account.clone();
                let fresh = tokio::task::spawn_blocking(move || {
                    let live = this.source.read_live()?;
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
        tokio::task::spawn_blocking(move || match this.source.read_live() {
            Ok(live) if account_matches_blob(&account, &live) => {
                this.capture_live_into_store(&live)?;
                let live = this.canonicalize_live_blob(live)?;
                Ok((live, QuerySource::LiveOwner))
            }
            Ok(_) => {
                let stored = this.stored_blob(&account)?;
                reject_foreign_credentials(&account, &stored)?;
                Ok((stored, QuerySource::ParkedConfirmed))
            }
            Err(_) => {
                let stored = this.stored_blob(&account)?;
                reject_foreign_credentials(&account, &stored)?;
                Ok((stored, QuerySource::LiveUnreadable))
            }
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
            let account = this.require_account(&account_id)?;
            let latest = this.stored_blob(&account)?;
            reject_foreign_credentials(&account, &latest)?;
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
        let (id, display_name) = match &self.source {
            CredentialSource::Desktop { .. } => ("cursor_desktop", "Cursor desktop credentials"),
            CredentialSource::Agent { .. } => ("cursor_agent", "Cursor CLI agent credentials"),
        };
        vec![ClientTarget {
            id: id.into(),
            display_name: display_name.into(),
            probe_path: self.source.probe_path(),
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

/// 解析默认凭证来源：显式覆盖优先；否则桌面库能读出有效登录时用桌面版。
/// 桌面未登录时回退命令行：文件后端的 auth.json，或 macOS 钥匙串里的 access token。
/// 两者都不存在时回退桌面路径，读取时给出「请先登录」提示。
fn default_credential_source() -> Result<CredentialSource> {
    if std::env::var_os(STATE_DB_ENV).is_none() {
        if let Some(auth) = std::env::var_os(AGENT_AUTH_ENV) {
            let auth_json = require_absolute(PathBuf::from(auth), AGENT_AUTH_ENV)?;
            let cli_config = match std::env::var_os(AGENT_CONFIG_ENV) {
                Some(path) => require_absolute(PathBuf::from(path), AGENT_CONFIG_ENV)?,
                None => default_agent_config_path()
                    .unwrap_or_else(|| auth_json.with_file_name("cli-config.json")),
            };
            return Ok(CredentialSource::Agent {
                auth_json,
                cli_config,
                token_store: AgentTokenStore::File,
            });
        }
    }
    let desktop = default_state_db_path()?;
    // 显式指定桌面数据库路径时始终走桌面版。
    if std::env::var_os(STATE_DB_ENV).is_some() {
        return Ok(CredentialSource::Desktop { state_db: desktop });
    }
    let agent = default_agent_source();
    Ok(select_credential_source(desktop, agent))
}

/// 找到 CLI 凭证时优先保留为候选来源。配置文件缺失不阻止导入：可从 JWT 解析身份。
fn default_agent_source() -> Option<CredentialSource> {
    let auth_json = default_agent_auth_path()?;
    let cli_config =
        default_agent_config_path().unwrap_or_else(|| auth_json.with_file_name("cli-config.json"));
    if agent_file_has_access_token(&auth_json) {
        return Some(CredentialSource::Agent {
            auth_json,
            cli_config,
            token_store: AgentTokenStore::File,
        });
    }
    #[cfg(target_os = "macos")]
    {
        let path = cursor_keychain_override();
        if agent_keychain_has_access_token(path.as_deref()) {
            return Some(CredentialSource::Agent {
                auth_json,
                cli_config,
                token_store: AgentTokenStore::Keychain { path },
            });
        }
    }
    None
}

/// 桌面数据库可能由未登录的旧安装留下。只有能读出有效凭证时才优先桌面端，
/// 否则回退到已登录的 Cursor CLI，避免默认入口静默遗漏该账号。
fn select_credential_source(desktop: PathBuf, agent: Option<CredentialSource>) -> CredentialSource {
    if desktop.exists() && (read_live_blob(&desktop).is_ok() || agent.is_none()) {
        return CredentialSource::Desktop { state_db: desktop };
    }
    agent.unwrap_or(CredentialSource::Desktop { state_db: desktop })
}

/// cursor-agent 登录文件路径。与官方 CLI 对齐：macOS 为 `~/.cursor/auth.json`，
/// Linux 为 `~/.config/cursor/auth.json`，Windows 为 `%APPDATA%/Cursor/auth.json`。
fn default_agent_auth_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".cursor/auth.json"))
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join("Cursor/auth.json"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".config/cursor/auth.json"))
    }
}

/// cursor-agent 的 cli-config.json 默认路径：`~/.cursor/cli-config.json`。
fn default_agent_config_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".cursor/cli-config.json"))
}

fn default_state_db_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(STATE_DB_ENV) {
        return validate_state_db_override(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = directories::BaseDirs::new() {
        return Ok(home
            .home_dir()
            .join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"));
    }
    #[cfg(target_os = "windows")]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Ok(PathBuf::from(appdata).join("Cursor/User/globalStorage/state.vscdb"));
    }
    #[cfg(target_os = "linux")]
    if let Some(home) = directories::BaseDirs::new() {
        return Ok(home
            .home_dir()
            .join(".config/Cursor/User/globalStorage/state.vscdb"));
    }
    Err(Error::Config(
        "cannot resolve the Cursor state database path".into(),
    ))
}

fn validate_state_db_override(path: PathBuf) -> Result<PathBuf> {
    require_absolute(path, STATE_DB_ENV)
}

fn require_absolute(path: PathBuf, env: &str) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::Config(format!("{env} must be an absolute path")));
    }
    Ok(path)
}

/// 从 cursor-agent 的 token 后端与 cli-config.json（authInfo 元数据）拼出账号凭证。
fn read_agent_blob(
    auth_json: &Path,
    cli_config: &Path,
    token_store: &AgentTokenStore,
) -> Result<CursorBlob> {
    let (access_token, refresh_token) = match token_store {
        AgentTokenStore::File => read_agent_file_tokens(auth_json)?,
        #[cfg(target_os = "macos")]
        AgentTokenStore::Keychain { path } => read_agent_keychain_tokens(path.as_deref())?,
    };
    agent_blob_from_tokens(access_token, refresh_token, cli_config)
}

fn read_agent_file_tokens(auth_json: &Path) -> Result<(String, Option<String>)> {
    if !auth_json.exists() {
        return Err(Error::Provider(format!(
            "Cursor CLI agent is not signed in ({} not found); run `cursor-agent login` first",
            auth_json.display()
        )));
    }
    let raw = std::fs::read_to_string(auth_json)
        .map_err(|error| Error::Provider(format!("read Cursor agent auth.json: {error}")))?;
    let auth: Value = serde_json::from_str(&raw)
        .map_err(|error| Error::Provider(format!("parse Cursor agent auth.json: {error}")))?;
    let access_token = auth
        .get("accessToken")
        .and_then(Value::as_str)
        .and_then(|token| non_empty(Some(token.to_string())))
        .ok_or_else(|| Error::Provider("Cursor agent auth.json is missing accessToken".into()))?;
    let refresh_token = auth
        .get("refreshToken")
        .and_then(Value::as_str)
        .and_then(|token| non_empty(Some(token.to_string())));
    Ok((access_token, refresh_token))
}

fn agent_file_has_access_token(auth_json: &Path) -> bool {
    read_agent_file_tokens(auth_json).is_ok()
}

fn agent_blob_from_tokens(
    access_token: String,
    refresh_token: Option<String>,
    cli_config: &Path,
) -> Result<CursorBlob> {
    // 邮箱、authId 等元数据在 cli-config.json 的 authInfo。若 authId 与令牌 JWT 不一致，
    // 说明身份文件是过期拼接，绝不能把当前令牌算到那个邮箱头上。
    let config = std::fs::read_to_string(cli_config)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    let auth_info = config.as_ref().and_then(|value| value.get("authInfo"));
    let info_email = auth_info
        .and_then(|value| value.get("email"))
        .and_then(Value::as_str)
        .and_then(|email| non_empty(Some(email.to_string())));
    let info_auth_id = auth_info
        .and_then(|value| value.get("authId"))
        .and_then(Value::as_str)
        .and_then(|id| non_empty(Some(id.to_string())));
    let info_membership = auth_info
        .and_then(|value| {
            value
                .get("membershipType")
                .or_else(|| value.get("stripeMembershipType"))
        })
        .and_then(Value::as_str)
        .map(str::to_string);
    let jwt_sub = jwt_subject(&access_token);
    let auth_info_agrees = match (&jwt_sub, &info_auth_id) {
        (Some(sub), Some(auth_id)) => auth_id == sub,
        _ => true,
    };
    let (email, auth_id, membership_type) = if auth_info_agrees {
        (
            info_email.or_else(|| jwt_sub.clone()),
            info_auth_id.or_else(|| jwt_sub.clone()),
            info_membership,
        )
    } else {
        (jwt_sub.clone(), jwt_sub, None)
    };
    let email = email.ok_or_else(|| {
        Error::Provider("cannot resolve Cursor agent account email or identity".into())
    })?;

    let blob = CursorBlob {
        access_token,
        refresh_token,
        email,
        auth_id,
        membership_type,
        subscription_status: None,
        sign_up_type: None,
    };
    validate_blob(&blob)?;
    Ok(blob)
}

/// 把账号凭证写回 cursor-agent 的 auth.json，保留文件中的其他字段。
fn write_agent_blob(auth_json: &Path, blob: &CursorBlob) -> Result<()> {
    let mut root = std::fs::read_to_string(auth_json)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Map<String, Value>>(&raw).ok())
        .unwrap_or_default();
    root.insert(
        "accessToken".into(),
        Value::String(blob.access_token.clone()),
    );
    match &blob.refresh_token {
        Some(refresh) => {
            root.insert("refreshToken".into(), Value::String(refresh.clone()));
        }
        None => {
            root.remove("refreshToken");
        }
    }
    if let Some(parent) = auth_json.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = open_private_file(auth_json)?;
    file.set_len(0)?;
    file.write_all(serde_json::to_string_pretty(&root)?.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// 把账号身份写回 cli-config.json 的 authInfo，保留文件中的其他字段。
fn write_agent_cli_config(cli_config: &Path, blob: &CursorBlob) -> Result<()> {
    let mut root = std::fs::read_to_string(cli_config)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Map<String, Value>>(&raw).ok())
        .unwrap_or_default();
    let mut auth_info = root
        .get("authInfo")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    auth_info.insert("email".into(), Value::String(blob.email.clone()));
    match &blob.auth_id {
        Some(auth_id) => {
            auth_info.insert("authId".into(), Value::String(auth_id.clone()));
        }
        None => {
            auth_info.remove("authId");
        }
    }
    if let Some(membership) = &blob.membership_type {
        auth_info.insert("membershipType".into(), Value::String(membership.clone()));
    }
    root.insert("authInfo".into(), Value::Object(auth_info));
    if let Some(parent) = cli_config.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = open_private_file(cli_config)?;
    file.set_len(0)?;
    file.write_all(serde_json::to_string_pretty(&root)?.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// 令牌与身份成套写入当前 agent 后端。
fn write_agent_live(
    auth_json: &Path,
    cli_config: &Path,
    token_store: &AgentTokenStore,
    blob: &CursorBlob,
) -> Result<()> {
    match token_store {
        AgentTokenStore::File => write_agent_blob(auth_json, blob)?,
        #[cfg(target_os = "macos")]
        AgentTokenStore::Keychain { path } => write_agent_keychain(path.as_deref(), blob)?,
    }
    write_agent_cli_config(cli_config, blob)
}

fn restore_bytes(path: &Path, previous: Option<&[u8]>) -> Result<()> {
    restore_agent_auth(path, previous)
}

/// 回滚 auth.json：有旧内容则还原，原本不存在则删除。
fn restore_agent_auth(auth_json: &Path, previous: Option<&[u8]>) -> Result<()> {
    match previous {
        Some(bytes) => {
            let mut file = open_private_file(auth_json)?;
            file.set_len(0)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        }
        None => match std::fs::remove_file(auth_json) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        },
    }
}

/// 命令行钥匙串快照，供切换失败回滚。不保存到用户可见输出。
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Default, Serialize)]
struct AgentKeychainSnapshot {
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// 命令行 token 快照：文件与钥匙串共用同一套回滚入口。
#[derive(Debug, Clone, Serialize)]
enum AgentTokenSnapshot {
    File {
        bytes: Option<Vec<u8>>,
    },
    #[cfg(target_os = "macos")]
    Keychain(AgentKeychainSnapshot),
}

fn snapshot_agent_tokens(
    auth_json: &Path,
    token_store: &AgentTokenStore,
) -> Result<AgentTokenSnapshot> {
    match token_store {
        AgentTokenStore::File => Ok(AgentTokenSnapshot::File {
            bytes: std::fs::read(auth_json).ok(),
        }),
        #[cfg(target_os = "macos")]
        AgentTokenStore::Keychain { path } => Ok(AgentTokenSnapshot::Keychain(
            snapshot_agent_keychain(path.as_deref())?,
        )),
    }
}

fn restore_agent_tokens(
    auth_json: &Path,
    token_store: &AgentTokenStore,
    previous: &AgentTokenSnapshot,
) -> Result<()> {
    match (token_store, previous) {
        (AgentTokenStore::File, AgentTokenSnapshot::File { bytes }) => {
            restore_agent_auth(auth_json, bytes.as_deref())
        }
        #[cfg(target_os = "macos")]
        (AgentTokenStore::Keychain { path }, AgentTokenSnapshot::Keychain(snapshot)) => {
            restore_agent_keychain(path.as_deref(), snapshot)
        }
        #[cfg(target_os = "macos")]
        _ => Err(Error::Provider(
            "Cursor CLI token snapshot does not match the current credential store".into(),
        )),
    }
}

/// 测试隔离用一次性钥匙串；生产环境不设，走登录钥匙串。
#[cfg(target_os = "macos")]
fn cursor_keychain_override() -> Option<PathBuf> {
    std::env::var_os(AGENT_KEYCHAIN_ENV)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// 与 Claude 相同：只 fork `/usr/bin/security`，避免 keyring crate 改写 ACL。
#[cfg(target_os = "macos")]
fn run_cursor_security(base: &[&str], keychain: Option<&Path>) -> Result<std::process::Output> {
    let mut command = Command::new("/usr/bin/security");
    command.args(base);
    if let Some(path) = keychain {
        command.arg(path);
    }
    command
        .output()
        .map_err(|error| Error::Credential(format!("run /usr/bin/security failed: {error}")))
}

#[cfg(target_os = "macos")]
fn security_find_cursor_password(service: &str, keychain: Option<&Path>) -> Result<Option<String>> {
    let output = run_cursor_security(
        &[
            "find-generic-password",
            "-s",
            service,
            "-a",
            AGENT_KEYCHAIN_ACCOUNT,
            "-w",
        ],
        keychain,
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    let mut raw = String::from_utf8(output.stdout)
        .map_err(|error| Error::Credential(format!("Cursor keychain non-UTF8: {error}")))?;
    if raw.ends_with('\n') {
        raw.pop();
    }
    Ok(non_empty(Some(raw)))
}

#[cfg(target_os = "macos")]
fn cursor_keychain_item_exists(service: &str, keychain: Option<&Path>) -> bool {
    run_cursor_security(
        &[
            "find-generic-password",
            "-s",
            service,
            "-a",
            AGENT_KEYCHAIN_ACCOUNT,
        ],
        keychain,
    )
    .map(|output| output.status.success())
    .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn cursor_app_bundle_path() -> Option<String> {
    let mut candidates = vec![PathBuf::from("/Applications/Cursor.app")];
    if let Some(dirs) = directories::BaseDirs::new() {
        candidates.push(dirs.home_dir().join("Applications/Cursor.app"));
    }
    candidates
        .into_iter()
        .find(|path| path.exists())
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn security_set_cursor_password(service: &str, value: &str, keychain: Option<&Path>) -> Result<()> {
    if cursor_keychain_item_exists(service, keychain) {
        // 已有条目只改内容、不动 ACL。删建会把解密权限收成「仅 security」，
        // 桌面版读自己的令牌会报未登录。
        let update = run_cursor_security(
            &[
                "add-generic-password",
                "-a",
                AGENT_KEYCHAIN_ACCOUNT,
                "-s",
                service,
                "-w",
                value,
                "-U",
            ],
            keychain,
        )?;
        if update.status.success() {
            return Ok(());
        }
        return Err(Error::Credential(format!(
            "update Cursor keychain item {service} failed: {}",
            String::from_utf8_lossy(&update.stderr)
        )));
    }

    let cursor_app = cursor_app_bundle_path();
    let mut args = vec![
        "add-generic-password",
        "-a",
        AGENT_KEYCHAIN_ACCOUNT,
        "-s",
        service,
        "-w",
        value,
        "-U",
        "-T",
        "/usr/bin/security",
    ];
    if let Some(app) = cursor_app.as_deref() {
        args.push("-T");
        args.push(app);
    }
    let add = run_cursor_security(&args, keychain)?;
    if add.status.success() {
        return Ok(());
    }
    Err(Error::Credential(format!(
        "write Cursor CLI keychain failed: {}",
        String::from_utf8_lossy(&add.stderr)
    )))
}

#[cfg(target_os = "macos")]
fn security_delete_cursor_password(service: &str, keychain: Option<&Path>) -> Result<()> {
    let _ = run_cursor_security(
        &[
            "delete-generic-password",
            "-s",
            service,
            "-a",
            AGENT_KEYCHAIN_ACCOUNT,
        ],
        keychain,
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn read_agent_keychain_tokens(keychain: Option<&Path>) -> Result<(String, Option<String>)> {
    let access = security_find_cursor_password(AGENT_ACCESS_SERVICE, keychain)?.ok_or_else(|| {
        Error::Provider(
            "Cursor CLI agent is not signed in (macOS keychain has no access token); run `cursor-agent login` first"
                .into(),
        )
    })?;
    let refresh = security_find_cursor_password(AGENT_REFRESH_SERVICE, keychain)?;
    Ok((access, refresh))
}

#[cfg(target_os = "macos")]
fn agent_keychain_has_access_token(keychain: Option<&Path>) -> bool {
    matches!(
        security_find_cursor_password(AGENT_ACCESS_SERVICE, keychain),
        Ok(Some(_))
    )
}

#[cfg(target_os = "macos")]
fn write_agent_keychain(keychain: Option<&Path>, blob: &CursorBlob) -> Result<()> {
    security_set_cursor_password(AGENT_ACCESS_SERVICE, &blob.access_token, keychain)?;
    match &blob.refresh_token {
        Some(refresh) => {
            security_set_cursor_password(AGENT_REFRESH_SERVICE, refresh, keychain)?;
        }
        None => {
            security_delete_cursor_password(AGENT_REFRESH_SERVICE, keychain)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn snapshot_agent_keychain(keychain: Option<&Path>) -> Result<AgentKeychainSnapshot> {
    Ok(AgentKeychainSnapshot {
        access_token: security_find_cursor_password(AGENT_ACCESS_SERVICE, keychain)?,
        refresh_token: security_find_cursor_password(AGENT_REFRESH_SERVICE, keychain)?,
    })
}

#[cfg(target_os = "macos")]
fn restore_agent_keychain(keychain: Option<&Path>, previous: &AgentKeychainSnapshot) -> Result<()> {
    match &previous.access_token {
        Some(access) => security_set_cursor_password(AGENT_ACCESS_SERVICE, access, keychain)?,
        None => security_delete_cursor_password(AGENT_ACCESS_SERVICE, keychain)?,
    }
    match &previous.refresh_token {
        Some(refresh) => {
            security_set_cursor_password(AGENT_REFRESH_SERVICE, refresh, keychain)?;
        }
        None => security_delete_cursor_password(AGENT_REFRESH_SERVICE, keychain)?,
    }
    Ok(())
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
    if let Some(sub) = jwt_subject(&blob.access_token) {
        return account_has_subject(account, &sub);
    }
    if let Some(auth_id) = &blob.auth_id {
        if account_has_subject(account, auth_id) {
            return true;
        }
    }
    account_identity_fields_match(account, blob)
}

fn account_has_subject(account: &Account, subject: &str) -> bool {
    account.extra.get("auth_id").and_then(Value::as_str) == Some(subject) || account.id.0 == subject
}

fn account_identity_fields_match(account: &Account, blob: &CursorBlob) -> bool {
    if let Some(auth_id) = &blob.auth_id {
        if account.extra.get("auth_id").and_then(Value::as_str) == Some(auth_id.as_str()) {
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

fn owner_email_matches(account: &Account, blob: &CursorBlob) -> bool {
    account
        .extra
        .get("email")
        .and_then(Value::as_str)
        .is_some_and(|email| email.eq_ignore_ascii_case(&blob.email))
}

fn credentials_belong_to_account(account: &Account, blob: &CursorBlob) -> bool {
    match jwt_subject(&blob.access_token) {
        Some(sub) => account_has_subject(account, &sub),
        None => true,
    }
}

fn reject_foreign_credentials(account: &Account, blob: &CursorBlob) -> Result<()> {
    if credentials_belong_to_account(account, blob) {
        return Ok(());
    }
    Err(Error::QuotaFetch(format!(
        "re-login required for cursor:{}; stored credentials belong to another Cursor account",
        account.id
    )))
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
            Err(error) if is_lock_contended(&error) => {
                if Instant::now() >= deadline {
                    return Err(Error::Provider(timeout_message.into()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn is_lock_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        // LockFileEx 的 FAIL_IMMEDIATELY 路径通常返回 ERROR_LOCK_VIOLATION (33)；
        // 某些文件系统/Windows 版本会返回 ERROR_IO_PENDING (997)。两者都表示
        // 锁正在被其他句柄持有，应进入同一有界等待，而不是当作永久 IO 故障。
        matches!(error.raw_os_error(), Some(33 | 997))
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
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

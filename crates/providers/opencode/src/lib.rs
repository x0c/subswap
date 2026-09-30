//! OpenCode 官方账号与 Go API key 分属两个 Provider。
//!
//! Go 部分复用文件型共享引擎保存 Key；V2 切换走官方凭证数据库和命令，V1 才改 `auth.json`；
//! Console 部分独立实现：不存 secret、不刷新、切换走官方命令、额度走 Console 接口。
//! API key 只允许手动切换；官方账号才进入自动切换候选池。

pub mod auth;
pub mod console;
pub mod paths;
pub mod usage;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use subswap_core::error::{Error, Result};
use subswap_core::{
    Account, AccountId, AccountRegistry, ClientTarget, CredentialStore, Provider, Quota,
};
use subswap_provider_common::{
    BlobMetadata, FileBlobProvider, FileBlobRuntime, IsolationSpec, RefreshOutcome,
};

pub const PROVIDER_ID: &str = "opencode";
pub const API_KEY_PROVIDER_ID: &str = "opencode-api-key";

/// OpenCode Go runtime：差异点只在路径、局部合并、API key 与额度查询。
#[derive(Clone, Copy)]
pub struct OpencodeRuntime;

#[async_trait]
impl FileBlobRuntime for OpencodeRuntime {
    fn id(&self) -> &'static str {
        API_KEY_PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "OpenCode API Key"
    }
    fn home(&self) -> PathBuf {
        paths::opencode_home()
    }
    fn live_cred_path(&self, home: &Path) -> PathBuf {
        paths::auth_json_path(home)
    }
    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        let mut metadata = auth::parse_metadata(blob);
        metadata.extra.insert("manual_only".into(), true.into());
        metadata
    }
    fn isolation(&self) -> IsolationSpec {
        IsolationSpec {
            env_var: "XDG_DATA_HOME",
            native_cli: "opencode",
        }
    }
    async fn refresh(&self, _blob: &str) -> Result<RefreshOutcome> {
        Ok(RefreshOutcome::Unsupported)
    }
    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>> {
        usage::fetch_quota(access_token, account).await
    }
    fn extract_blob(&self, live_contents: &str) -> Option<String> {
        auth::extract_blob(live_contents)
    }
    fn compose_live(&self, existing_live: Option<&str>, blob: &str) -> String {
        auth::compose_live(existing_live, blob)
    }
    fn access_token(&self, blob: &str) -> Option<String> {
        auth::api_key_from_blob(blob)
    }
    fn isolation_rel_path(&self) -> Option<PathBuf> {
        Some(PathBuf::from("opencode").join("auth.json"))
    }
    fn isolation_extra_env(&self, composed_live: &str) -> Vec<(String, String)> {
        vec![("OPENCODE_AUTH_CONTENT".into(), composed_live.to_string())]
    }
}

/// OpenCode 官方 Console 账号；Go 引擎单独注册为 API Key Provider。
pub struct OpencodeProvider {
    go: Arc<FileBlobProvider<OpencodeRuntime>>,
    registry: Arc<AccountRegistry>,
}

/// Go API key 独立账号池。V2 从官方数据库同步并经官方命令切换；V1 使用 auth.json。
pub struct OpencodeApiKeyProvider {
    engine: Arc<FileBlobProvider<OpencodeRuntime>>,
    registry: Arc<AccountRegistry>,
}

impl OpencodeApiKeyProvider {
    pub fn new(
        engine: Arc<FileBlobProvider<OpencodeRuntime>>,
        registry: Arc<AccountRegistry>,
    ) -> Self {
        Self { engine, registry }
    }

    pub fn live_account_id(&self) -> Result<AccountId> {
        match console::read_v2_go_keys(&self.engine.home())? {
            Some(keys) => keys
                .into_iter()
                .find(|k| k.active)
                .map(|k| go_id_for_key(&k.key))
                .ok_or_else(|| {
                    Error::Provider("no active OpenCode Go API key in the official client".into())
                }),
            None => self.engine.live_account_id(),
        }
    }

    pub fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
        self.sync_active_metadata(label_hint)
    }

    pub fn sync_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        self.sync_accounts(label_hint)?
            .into_iter()
            .find(|a| a.active)
            .ok_or_else(|| {
                Error::Provider("no active OpenCode Go API key in the official client".into())
            })
    }

    /// 导入全部 V2 Key；即使当前没有选中 Key，也让它们可见并可查余量。
    pub fn sync_accounts(&self, label_hint: Option<String>) -> Result<Vec<Account>> {
        match console::read_v2_go_keys(&self.engine.home())? {
            Some(keys) => {
                let mut active = None;
                let mut accounts = Vec::new();
                for key in keys {
                    let account = self.engine.import_raw(
                        blob_from_key(&key.key),
                        if key.active { label_hint.clone() } else { None },
                        Some(key.active),
                    )?;
                    if key.active && active.is_none() {
                        active = Some(account.clone());
                    }
                    accounts.push(account);
                }
                if let Some(account) = &active {
                    self.registry.set_active(API_KEY_PROVIDER_ID, &account.id)?;
                } else {
                    self.clear_active_keys()?;
                }
                Ok(accounts)
            }
            None => match self.engine.live_account_id() {
                Ok(_) => self
                    .engine
                    .sync_active_metadata(label_hint)
                    .map(|a| vec![a]),
                Err(_) => {
                    self.clear_active_keys()?;
                    Ok(Vec::new())
                }
            },
        }
    }

    fn clear_active_keys(&self) -> Result<()> {
        let mut accounts = self.registry.load()?;
        let mut changed = false;
        for account in &mut accounts {
            if account.provider == API_KEY_PROVIDER_ID && account.active {
                account.active = false;
                changed = true;
            }
        }
        if changed {
            self.registry.save(&accounts)?;
        }
        Ok(())
    }

    pub fn import_raw(
        &self,
        raw: String,
        label_hint: Option<String>,
        active: Option<bool>,
    ) -> Result<Account> {
        self.engine.import_raw(raw, label_hint, active)
    }

    /// `rm` 用：先断官方，再由调用方清本地。阻塞 IO（SQLite/子进程/文件）包进
    /// `spawn_blocking`，遵守 async 内不直接阻塞 IO 的不变量。
    pub async fn disconnect_official(&self, id: &AccountId) -> Result<OfficialDisconnect> {
        let home = self.engine.home();
        let id = id.clone();
        tokio::task::spawn_blocking(move || disconnect_go_key_official(&home, &id))
            .await
            .map_err(|e| Error::Provider(format!("OpenCode Go disconnect join failed: {e}")))?
    }
}

/// V2 从官方数据库定位该 Key 并经官方 `auth logout` 断开；V1 只在 live 文件的
/// `opencode-go` 项正好是这把 Key 时清除该项（parked V1 Key 不在 live 里，本地删即可）。
fn disconnect_go_key_official(home: &Path, id: &AccountId) -> Result<OfficialDisconnect> {
    match console::read_v2_go_keys(home)? {
        Some(keys) => {
            let Some(hit) = keys.iter().find(|k| go_id_for_key(&k.key) == *id) else {
                return Ok(OfficialDisconnect::AlreadyGone);
            };
            let credential_id = hit.credential_id.clone();
            // 注意：官方集成名是 `opencode-go`（`auth::AUTH_SLOT`），不是 subswap
            // 内部的 provider id `opencode-api-key`。
            console::logout_credential(auth::AUTH_SLOT, &credential_id, home).map_err(|e| {
                Error::Provider(format!(
                    "cannot disconnect official OpenCode Go credential; \
                     run `opencode auth logout opencode-go {credential_id}` manually, \
                     then re-run rm: {e}"
                ))
            })?;
            let still = console::read_v2_go_keys(home)?
                .unwrap_or_default()
                .iter()
                .any(|k| go_id_for_key(&k.key) == *id);
            if still {
                return Err(Error::Provider(format!(
                    "official client still lists OpenCode Go credential {credential_id} \
                     after logout; disconnect it in the official client first"
                )));
            }
            Ok(OfficialDisconnect::Disconnected)
        }
        None => remove_v1_go_slot(home, id),
    }
}

/// V1：live 文件的 `opencode-go` 项是目标 Key 才清除该项（保留其他供应商），
/// 加锁后重读-改-写，避免与并发 `activate` 互盖。
fn remove_v1_go_slot(home: &Path, id: &AccountId) -> Result<OfficialDisconnect> {
    let live_path = paths::auth_json_path(home);
    let content = match std::fs::read_to_string(&live_path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(OfficialDisconnect::AlreadyGone);
        }
        Err(e) => {
            return Err(Error::Provider(format!(
                "read OpenCode live {}: {e}",
                live_path.display()
            )));
        }
    };
    let is_target = auth::extract_blob(&content).is_some_and(|blob| {
        auth::parse_metadata(&blob).primary_id.as_deref() == Some(id.0.as_str())
    });
    if !is_target {
        return Ok(OfficialDisconnect::AlreadyGone);
    }
    let lock_path = home.join(".subswap.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| Error::Provider(format!("open OpenCode lock {}: {e}", lock_path.display())))?;
    fs2::FileExt::lock_exclusive(&lock_file)
        .map_err(|e| Error::Provider(format!("lock OpenCode credentials: {e}")))?;
    let content = std::fs::read_to_string(&live_path)
        .map_err(|e| Error::Provider(format!("read OpenCode live {}: {e}", live_path.display())))?;
    let mut map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&content).unwrap_or_default();
    let still_target = map.get(auth::AUTH_SLOT).is_some_and(|entry| {
        auth::parse_metadata(&entry.to_string())
            .primary_id
            .as_deref()
            == Some(id.0.as_str())
    });
    if !still_target {
        return Ok(OfficialDisconnect::AlreadyGone);
    }
    map.remove(auth::AUTH_SLOT);
    write_live_atomic(&live_path, &serde_json::Value::Object(map).to_string())?;
    Ok(OfficialDisconnect::Disconnected)
}

/// 原子写 live 凭证：tmp + rename + 0o600（与共享引擎 `write_blob` 同策略）。
fn write_live_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::Provider(format!("create OpenCode dir {}: {e}", parent.display()))
        })?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, contents)
        .map_err(|e| Error::Provider(format!("write OpenCode live {}: {e}", tmp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| Error::Provider(format!("chmod OpenCode live {}: {e}", tmp.display())))?;
    }
    std::fs::rename(&tmp, path)
        .map_err(|e| Error::Provider(format!("replace OpenCode live {}: {e}", path.display())))?;
    Ok(())
}

fn go_id_for_key(key: &str) -> AccountId {
    AccountId(auth::fingerprint(key))
}

/// `rm` 断开官方凭证的结果（见 [`subswap_core::OfficialDisconnect`]，此处重导出保持旧引用可用）。
pub use subswap_core::OfficialDisconnect;

#[async_trait]
impl Provider for OpencodeApiKeyProvider {
    fn id(&self) -> &'static str {
        API_KEY_PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "OpenCode API Key"
    }
    fn client_targets(&self) -> Vec<ClientTarget> {
        let mut targets = self.engine.client_targets();
        targets.push(ClientTarget {
            id: format!("{API_KEY_PROVIDER_ID}_database"),
            display_name: "OpenCode Go credentials".into(),
            probe_path: console::console_db_path(&self.engine.home()),
        });
        targets
    }
    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(API_KEY_PROVIDER_ID)
    }
    async fn activate(&self, id: &AccountId) -> Result<()> {
        let home = self.engine.home();
        let keys = tokio::task::spawn_blocking(move || console::read_v2_go_keys(&home))
            .await
            .map_err(|e| {
                Error::Provider(format!("OpenCode Go credential read join failed: {e}"))
            })??;
        let Some(keys) = keys else {
            return self.engine.activate(id).await;
        };
        let previous = keys
            .iter()
            .find(|k| k.active)
            .map(|k| k.credential_id.clone());
        let credential = keys.into_iter().find(|k| go_id_for_key(&k.key) == *id)
            .ok_or_else(|| Error::Provider(format!(
                "OpenCode API key {id} is not connected in V2; run `opencode auth login opencode-go` first"
            )))?;
        let home = self.engine.home();
        let credential_id = credential.credential_id;
        let expected = id.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                console::switch_to("opencode-go", &credential_id, 2, &home)?;
                let selected = console::read_v2_go_keys(&home)?
                    .unwrap_or_default()
                    .into_iter()
                    .find(|k| k.active)
                    .map(|k| go_id_for_key(&k.key));
                if selected.as_ref() != Some(&expected) {
                    return Err(Error::Provider(format!(
                        "OpenCode did not select API key {expected}"
                    )));
                }
                Ok(())
            })();
            if result.is_err() {
                if let Some(old) = previous.filter(|old| *old != credential_id) {
                    console::switch_to("opencode-go", &old, 2, &home)?;
                }
            }
            result
        })
        .await
        .map_err(|e| Error::Provider(format!("OpenCode Go switch join failed: {e}")))??;
        let registry = self.registry.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || registry.set_active(API_KEY_PROVIDER_ID, &id))
            .await
            .map_err(|e| {
                Error::Provider(format!("OpenCode Go registry update join failed: {e}"))
            })??;
        Ok(())
    }
    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        self.engine.query_quota(id).await
    }
}

impl OpencodeProvider {
    fn go_home(&self) -> PathBuf {
        self.go.home()
    }

    fn require_account(&self, id: &AccountId) -> Result<Account> {
        self.registry
            .find(PROVIDER_ID, id)?
            .ok_or_else(|| Error::AccountNotFound {
                provider: PROVIDER_ID.into(),
                id: id.to_string(),
            })
    }

    /// 单独注册到 Provider 列表的 Go API Key 引擎。
    pub fn go_engine(&self) -> Arc<FileBlobProvider<OpencodeRuntime>> {
        self.go.clone()
    }

    // -- Console 官方账号 --

    /// 当前官方 Console 登录对应的 subswap 账号 id。未登录 → Err。
    pub fn live_console_id(&self) -> Result<AccountId> {
        let live = console::read_console_live(&self.go_home())?;
        live.map(|l| console::account_id_for(&l)).ok_or_else(|| {
            Error::Provider("no OpenCode Console login; run `subswap login opencode`".into())
        })
    }

    /// 导入当前官方 Console 登录（只写元数据，不存 secret），并标 active。
    pub fn import_console_active(&self, label_hint: Option<String>) -> Result<Account> {
        self.sync_console_accounts(label_hint)?.ok_or_else(|| {
            Error::Provider(
                "no OpenCode Console login found; run `opencode auth login opencode` (V2) \
                 or `opencode console login` (V1) first"
                    .into(),
            )
        })
    }

    /// 同步官方已保存的全部 Console 凭证。停用账号也进入账号池，额度查询使用各自凭证。
    pub fn sync_console_accounts(&self, label_hint: Option<String>) -> Result<Option<Account>> {
        let lives = console::read_console_accounts(&self.go_home())?;
        let mut active = None;
        for live in lives {
            let id = console::account_id_for(&live);
            let legacy_id = AccountId(format!("console-{}", live.org_id));
            let existing = self
                .registry
                .find(PROVIDER_ID, &id)?
                .or(self.registry.find(PROVIDER_ID, &legacy_id)?);
            let mut account = console::account_from_live(&live, existing.as_ref());
            account.active = live.active;
            if live.active {
                if let Some(hint) = label_hint.as_ref().filter(|s| !s.trim().is_empty()) {
                    account.label = hint.clone();
                }
            }
            self.registry.upsert(account.clone())?;
            if legacy_id != id && self.registry.find(PROVIDER_ID, &legacy_id)?.is_some() {
                self.registry.remove(PROVIDER_ID, &legacy_id)?;
            }
            if live.active && active.is_none() {
                active = Some(account);
            }
        }
        if let Some(account) = &active {
            self.registry.set_active(PROVIDER_ID, &account.id)?;
        }
        Ok(active)
    }

    /// 只对齐当前 Console 登录的元数据 active 标记（默认入口用）。
    pub fn sync_console_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        self.import_console_active(label_hint)
    }

    /// `rm` 用：V2 经官方 `auth logout opencode` 断开对应凭证；官方已无此账号
    /// 则直接清本地（幂等）；V1 无官方登出命令，返回 `Unsupported` 由调用方只清本地。
    pub async fn disconnect_official_console(
        &self,
        account: &Account,
    ) -> Result<OfficialDisconnect> {
        let home = self.go_home();
        let account_id = account.id.clone();
        let recorded_credential = account
            .extra
            .get("credential_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let lives = console::read_console_accounts(&home)?;
            let live = lives
                .into_iter()
                .find(|l| console::account_id_for(l) == account_id);
            let Some(live) = live else {
                return Ok(OfficialDisconnect::AlreadyGone);
            };
            let credential_id = recorded_credential.unwrap_or(live.credential_id);
            match console::detect_major_version() {
                Ok(major) if major < 2 => Ok(OfficialDisconnect::Unsupported),
                Ok(_) => {
                    console::logout_credential(PROVIDER_ID, &credential_id, &home).map_err(
                        |e| {
                            Error::Provider(format!(
                                "cannot disconnect official OpenCode Console credential; \
                             run `opencode auth logout opencode {credential_id}` manually, \
                             then re-run rm: {e}"
                            ))
                        },
                    )?;
                    let still = console::read_console_accounts(&home)?
                        .into_iter()
                        .any(|l| console::account_id_for(&l) == account_id);
                    if still {
                        return Err(Error::Provider(format!(
                            "official client still lists Console account {account_id} \
                             after logout; disconnect it in the official client first"
                        )));
                    }
                    Ok(OfficialDisconnect::Disconnected)
                }
                Err(e) => Err(Error::Provider(format!(
                    "cannot disconnect official OpenCode Console credential \
                     (`opencode` binary unavailable: {e}); run \
                     `opencode auth logout opencode {credential_id}` manually, then re-run rm"
                ))),
            }
        })
        .await
        .map_err(|e| Error::Provider(format!("OpenCode Console disconnect join failed: {e}")))?
    }

    /// 经官方命令切到指定 Console 账号。阻塞子进程，调用方已在 async 上下文时
    /// 由本函数内部转入 `spawn_blocking`。
    pub async fn activate_console(&self, account: &Account) -> Result<()> {
        let credential_id = account
            .extra
            .get("credential_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Provider(format!(
                    "console account {} has no credential id; re-import it",
                    account.id
                ))
            })?
            .to_string();
        let home = self.go_home();
        let expected = account.id.clone();
        tokio::task::spawn_blocking(move || {
            let previous = console::read_console_live(&home)?.map(|live| live.credential_id);
            let major = console::detect_major_version()?;
            let result = (|| {
                console::switch_to(PROVIDER_ID, &credential_id, major, &home)?;
                let selected =
                    console::read_console_live(&home)?.map(|live| console::account_id_for(&live));
                if selected.as_ref() != Some(&expected) {
                    return Err(Error::Provider(format!(
                        "OpenCode did not select Console account {expected}"
                    )));
                }
                Ok(())
            })();
            if result.is_err() {
                if let Some(old) = previous.filter(|old| *old != credential_id) {
                    console::switch_to(PROVIDER_ID, &old, major, &home)?;
                }
            }
            result
        })
        .await
        .map_err(|e| Error::Provider(format!("console switch join failed: {e}")))??;
        let registry = self.registry.clone();
        let id = account.id.clone();
        tokio::task::spawn_blocking(move || registry.set_active(PROVIDER_ID, &id))
            .await
            .map_err(|e| Error::Provider(format!("Console registry update join failed: {e}")))??;
        Ok(())
    }

    async fn query_console_quota(&self, account: &Account) -> Result<Vec<Quota>> {
        let home = self.go_home();
        let lives = tokio::task::spawn_blocking(move || console::read_console_accounts(&home))
            .await
            .map_err(|e| Error::Provider(format!("console credential read join failed: {e}")))??;
        let live = lives
            .into_iter()
            .find(|live| console::account_id_for(live) == account.id)
            .ok_or_else(|| {
                Error::QuotaFetch(
                    "OpenCode Console credential is no longer available; needs re-login".into(),
                )
            })?;
        console::fetch_console_quota(&live, account).await
    }
}

#[async_trait]
impl Provider for OpencodeProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "OpenCode"
    }
    fn client_targets(&self) -> Vec<ClientTarget> {
        vec![ClientTarget {
            id: format!("{PROVIDER_ID}_console"),
            display_name: "OpenCode Console login".into(),
            probe_path: console::console_db_path(&self.go_home()),
        }]
    }
    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(PROVIDER_ID)
    }
    async fn activate(&self, id: &AccountId) -> Result<()> {
        let account = self.require_account(id)?;
        self.activate_console(&account).await
    }
    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        let account = self.require_account(id)?;
        self.query_console_quota(&account).await
    }
}

/// 构造 OpenCodeProvider。
pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> OpencodeProvider {
    OpencodeProvider {
        go: Arc::new(FileBlobProvider::new(
            OpencodeRuntime,
            store,
            registry.clone(),
        )),
        registry,
    }
}

/// 把旧版混在 `opencode` 下的 Go key 移到独立账号池，保留账号和凭证。
/// 先复制 secret，再原子保存 registry；重复运行不会产生重复账号。
pub fn migrate_legacy_api_keys(
    store: &dyn CredentialStore,
    registry: &AccountRegistry,
) -> Result<()> {
    let mut accounts = registry.load()?;
    let legacy: Vec<Account> = accounts
        .iter()
        .filter(|a| a.provider == PROVIDER_ID && !console::is_console_account(a))
        .cloned()
        .collect();
    if legacy.is_empty() {
        return Ok(());
    }
    for old in &legacy {
        let already_migrated = accounts
            .iter()
            .any(|a| a.provider == API_KEY_PROVIDER_ID && a.id == old.id);
        if !already_migrated {
            if let Some(blob) = store.get(PROVIDER_ID, &old.id.0, "blob")? {
                store.set(API_KEY_PROVIDER_ID, &old.id.0, "blob", &blob)?;
            }
            let mut moved = old.clone();
            moved.provider = API_KEY_PROVIDER_ID.into();
            moved.extra.insert("manual_only".into(), true.into());
            accounts.push(moved);
        }
    }
    for account in &mut accounts {
        if account.provider == API_KEY_PROVIDER_ID {
            account.extra.insert("manual_only".into(), true.into());
        }
    }
    accounts.retain(|a| !(a.provider == PROVIDER_ID && !console::is_console_account(a)));
    registry.save(&accounts)?;
    for old in legacy {
        store.delete(PROVIDER_ID, &old.id.0, "blob")?;
    }
    Ok(())
}

/// 由粘贴的 API key 生成可导入的 blob。
pub fn blob_from_key(key: &str) -> String {
    auth::blob_from_key(key.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use subswap_core::FileStore;

    #[test]
    fn v1_disconnect_clears_live_slot_and_keeps_neighbors() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let target_key = "sk-test-rm-target-0001";
        let target_id = AccountId(auth::fingerprint(target_key));
        std::fs::write(
            paths::auth_json_path(home),
            serde_json::json!({
                "openai": {"type": "api", "key": "sk-keep"},
                "opencode-go": {"type": "api", "key": target_key},
            })
            .to_string(),
        )
        .unwrap();

        let out = disconnect_go_key_official(home, &target_id).unwrap();
        assert_eq!(out, OfficialDisconnect::Disconnected);

        let live: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(paths::auth_json_path(home)).unwrap())
                .unwrap();
        assert_eq!(live["openai"]["key"], "sk-keep");
        assert!(live.get("opencode-go").is_none());

        assert_eq!(
            disconnect_go_key_official(home, &target_id).unwrap(),
            OfficialDisconnect::AlreadyGone
        );
    }

    #[test]
    fn v1_disconnect_leaves_other_live_key_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let before = serde_json::json!({
            "opencode-go": {"type": "api", "key": "sk-test-other-0002"},
        })
        .to_string();
        std::fs::write(paths::auth_json_path(home), &before).unwrap();

        let other_id = AccountId(auth::fingerprint("sk-test-unrelated-0003"));
        let out = disconnect_go_key_official(home, &other_id).unwrap();
        assert_eq!(out, OfficialDisconnect::AlreadyGone);
        assert_eq!(
            std::fs::read_to_string(paths::auth_json_path(home)).unwrap(),
            before
        );
    }

    #[test]
    fn migrates_go_keys_without_moving_console_accounts() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
        let store = FileStore::new(tmp.path().join("credentials.json"));
        let go = Account {
            provider: PROVIDER_ID.into(),
            id: AccountId("go_1".into()),
            label: "sk-…test".into(),
            active: true,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        };
        let mut console = go.clone();
        console.id = AccountId("console-user-wrk".into());
        console.extra.insert("kind".into(), "console".into());
        registry.save(&[go, console.clone()]).unwrap();
        store
            .set(PROVIDER_ID, "go_1", "blob", &blob_from_key("sk-test-key"))
            .unwrap();

        migrate_legacy_api_keys(&store, &registry).unwrap();
        migrate_legacy_api_keys(&store, &registry).unwrap();

        let accounts = registry.load().unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(registry.find(PROVIDER_ID, &console.id).unwrap().is_some());
        let moved = registry
            .find(API_KEY_PROVIDER_ID, &AccountId("go_1".into()))
            .unwrap()
            .unwrap();
        assert!(moved.manual_only());
        assert!(store
            .get(API_KEY_PROVIDER_ID, "go_1", "blob")
            .unwrap()
            .is_some());
        assert!(store.get(PROVIDER_ID, "go_1", "blob").unwrap().is_none());
    }
}

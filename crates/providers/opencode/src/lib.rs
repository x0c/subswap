//! OpenCode Provider：Go API key（文件型）+ Console 官方登录（SQLite + 官方命令）双账号。
//!
//! Go 部分沿用文件型共享引擎（只切换 `auth.json` 的 `opencode-go` 条目）；
//! Console 部分独立实现：不存 secret、不刷新、切换走官方命令、额度走 Console 接口。
//! 两类账号按 [`console::is_console_account`] 区分，`activate` / `query_quota` 据此路由。

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
    BlobMetadata, FileBlobProvider, FileBlobRuntime, IsolatedProvider, IsolationSpec,
    RefreshOutcome,
};

pub const PROVIDER_ID: &str = "opencode";

/// OpenCode Go runtime：差异点只在路径、局部合并、API key 与额度查询。
#[derive(Clone, Copy)]
pub struct OpencodeRuntime;

#[async_trait]
impl FileBlobRuntime for OpencodeRuntime {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "OpenCode Go"
    }
    fn home(&self) -> PathBuf {
        paths::opencode_home()
    }
    fn live_cred_path(&self, home: &Path) -> PathBuf {
        paths::auth_json_path(home)
    }
    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        auth::parse_metadata(blob)
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

/// OpenCode Provider：Go 文件引擎 + Console 官方账号。
pub struct OpencodeProvider {
    go: Arc<FileBlobProvider<OpencodeRuntime>>,
    registry: Arc<AccountRegistry>,
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

    // -- Go 透传（既有行为保持不变） --

    /// daemon 文件 reconcile 用内部 Go 引擎句柄。
    pub fn go_engine(&self) -> Arc<FileBlobProvider<OpencodeRuntime>> {
        self.go.clone()
    }

    pub fn live_account_id(&self) -> Result<AccountId> {
        self.go.live_account_id()
    }

    pub fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
        self.go.import_active(label_hint)
    }

    pub fn sync_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        self.go.sync_active_metadata(label_hint)
    }

    pub fn import_raw(
        &self,
        raw: String,
        label_hint: Option<String>,
        active: Option<bool>,
    ) -> Result<Account> {
        self.go.import_raw(raw, label_hint, active)
    }

    // -- Console 官方账号 --

    /// 当前官方 Console 登录对应的 subswap 账号 id。未登录 → Err。
    pub fn live_console_id(&self) -> Result<AccountId> {
        let live = console::read_console_live(&self.go_home())?;
        live.map(|l| console::account_id_for(&l.org_id))
            .ok_or_else(|| {
                Error::Provider("no OpenCode Console login; run `subswap login opencode`".into())
            })
    }

    /// 导入当前官方 Console 登录（只写元数据，不存 secret），并标 active。
    pub fn import_console_active(&self, label_hint: Option<String>) -> Result<Account> {
        let live = console::read_console_live(&self.go_home())?.ok_or_else(|| {
            Error::Provider(
                "no OpenCode Console login found; run `opencode auth login opencode` (V2) \
                 or `opencode console login` (V1) first"
                    .into(),
            )
        })?;
        let id = console::account_id_for(&live.org_id);
        let existing = self.registry.find(PROVIDER_ID, &id)?;
        let mut account = console::account_from_live(&live, existing.as_ref());
        if let Some(hint) = label_hint {
            if !hint.trim().is_empty() {
                account.label = hint;
            }
        }
        self.registry.upsert(account.clone())?;
        self.registry.set_active(PROVIDER_ID, &id)?;
        Ok(account)
    }

    /// 只对齐当前 Console 登录的元数据 active 标记（默认入口用）。
    pub fn sync_console_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        self.import_console_active(label_hint)
    }

    /// 经官方命令切到指定 Console 账号。阻塞子进程，调用方已在 async 上下文时
    /// 由本函数内部转入 `spawn_blocking`。
    pub async fn activate_console(&self, account: &Account) -> Result<()> {
        let label = account
            .extra
            .get("credential_label")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Provider(format!(
                    "console account {} has no credential label; re-import it",
                    account.id
                ))
            })?
            .to_string();
        let home = self.go_home();
        tokio::task::spawn_blocking(move || {
            let major = console::detect_major_version()?;
            console::switch_to(&label, major, &home)
        })
        .await
        .map_err(|e| Error::Provider(format!("console switch join failed: {e}")))??;
        self.registry.set_active(PROVIDER_ID, &account.id)?;
        Ok(())
    }

    async fn query_console_quota(&self, account: &Account) -> Result<Vec<Quota>> {
        let home = self.go_home();
        let live = tokio::task::spawn_blocking(move || console::read_console_live(&home))
            .await
            .map_err(|e| Error::Provider(format!("console credential read join failed: {e}")))??;
        let live = live
            .ok_or_else(|| Error::QuotaFetch("no OpenCode Console login; needs re-login".into()))?;
        let expected = console::account_id_for(&live.org_id);
        if expected != account.id {
            return Err(Error::QuotaFetch(format!(
                "console login moved to {} (expected {}); needs re-login",
                expected, account.id
            )));
        }
        console::fetch_console_quota(&live, account).await
    }
}

#[async_trait]
impl Provider for OpencodeProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "OpenCode Go"
    }
    fn client_targets(&self) -> Vec<ClientTarget> {
        let mut targets = vec![ClientTarget {
            id: format!("{PROVIDER_ID}_live"),
            display_name: "OpenCode Go credentials".into(),
            probe_path: paths::auth_json_path(&self.go_home()),
        }];
        targets.push(ClientTarget {
            id: format!("{PROVIDER_ID}_console"),
            display_name: "OpenCode Console login".into(),
            probe_path: console::console_db_path(&self.go_home()),
        });
        targets
    }
    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(PROVIDER_ID)
    }
    async fn activate(&self, id: &AccountId) -> Result<()> {
        let account = self.require_account(id)?;
        if console::is_console_account(&account) {
            return self.activate_console(&account).await;
        }
        self.go.activate(id).await
    }
    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        let account = self.require_account(id)?;
        if console::is_console_account(&account) {
            return self.query_console_quota(&account).await;
        }
        self.go.query_quota(id).await
    }
}

impl IsolatedProvider for OpencodeProvider {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn isolation_env_var(&self) -> &'static str {
        self.go.isolation().env_var
    }
    fn native_cli(&self) -> &'static str {
        self.go.isolation().native_cli
    }
    fn materialize(&self, id: &AccountId, env_dir: &Path) -> Result<()> {
        let account = self.require_account(id)?;
        if console::is_console_account(&account) {
            return Err(Error::Provider(
                "isolated runs are not supported for OpenCode Console accounts \
                 (credentials live in the official client database)"
                    .into(),
            ));
        }
        self.go.materialize(id, env_dir)
    }
    fn absorb(&self, id: &AccountId, env_dir: &Path) -> Result<()> {
        let account = self.require_account(id)?;
        if console::is_console_account(&account) {
            return Err(Error::Provider(
                "isolated runs are not supported for OpenCode Console accounts".into(),
            ));
        }
        self.go.absorb(id, env_dir)
    }
    fn isolation_extra_env(&self, id: &AccountId) -> Vec<(String, String)> {
        self.require_account(id)
            .ok()
            .filter(|a| !console::is_console_account(a))
            .map(|_| self.go.isolation_extra_env(id))
            .unwrap_or_default()
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

/// 由粘贴的 API key 生成可导入的 blob。
pub fn blob_from_key(key: &str) -> String {
    auth::blob_from_key(key.trim())
}

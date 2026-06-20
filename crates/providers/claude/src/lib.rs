//! Claude / Anthropic Provider 实现。
//!
//! 关键约束（来自 docs/design/AUTO_SWAP_DESIGN.md）：
//! - `activate` 路径**不强依赖**网络：token 预刷新是 best-effort，失败仅打 warn 不阻塞切换。
//! - 切换流程：flock → snapshot → 写凭证 → 写 oauthAccount → 释放锁；任一步失败回滚。
//! - 同步阻塞 IO（flock + 文件读写 + keyring）全部放在 [`tokio::task::spawn_blocking`] 里，
//!   避免堵塞 tokio worker（daemon 并发 activate 时尤其重要）。
//! - 敏感数据：credentials.json 整段写 keyring；registry.toml 只存元数据。

mod claude_files;
mod oauth;
mod paths;

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;

use subswap_core::error::{Error, Result};
use subswap_core::swap::{swap_with_snapshot, SwapTarget};
use subswap_core::{
    Account, AccountId, AccountRegistry, ClientTarget, CredentialStore, Provider, Quota,
    QuotaStatus, QuotaWindow,
};

use crate::claude_files::{
    capture_managed_env, mark_onboarding_complete, read_api_state, read_credentials,
    read_oauth_account, read_settings, remove_api_state, restore_oauth_env_in_settings,
    write_api_env_into_settings, write_api_state, write_credentials,
    write_oauth_account_into_global, ApiState, CredentialsFile, OauthAccount, MANAGED_API_ENV_KEYS,
};
use crate::paths::{
    api_state_path, claude_home, credentials_path, global_config_path, settings_path,
};

/// 凭证字段名：整段 credentials.json 的 JSON 序列化结果。
const CRED_FIELD: &str = "credentials_json";
/// 自定义 API 密钥字段名。
pub const API_KEY_FIELD: &str = "api_key";
/// Provider 标识。
pub const PROVIDER_ID: &str = "claude";
const ACCOUNT_KIND_FIELD: &str = "kind";
const API_CONFIG_FIELD: &str = "api_config";
const API_KIND: &str = "api";
// 数值调优参数运行时取自 [`subswap_core::settings::current`]；config.toml 即时生效。
use subswap_core::settings;

/// Claude Code 自定义 API 配置。密钥不在这里，由 [`CredentialStore`] 单独保存。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClaudeApiConfig {
    pub base_url: String,
    /// `ANTHROPIC_AUTH_TOKEN` 或 `ANTHROPIC_API_KEY`。
    pub auth_field: String,
    pub model: String,
    pub opus_model: String,
    pub sonnet_model: String,
    pub haiku_model: String,
    pub subagent_model: String,
    pub effort_level: String,
}

impl ClaudeApiConfig {
    fn env(&self, api_key: &str) -> Result<serde_json::Map<String, serde_json::Value>> {
        if !matches!(
            self.auth_field.as_str(),
            "ANTHROPIC_AUTH_TOKEN" | "ANTHROPIC_API_KEY"
        ) {
            return Err(Error::Config(format!(
                "unsupported Claude API auth field: {}",
                self.auth_field
            )));
        }
        let mut env = serde_json::Map::new();
        for (key, value) in [
            ("ANTHROPIC_BASE_URL", self.base_url.as_str()),
            (self.auth_field.as_str(), api_key),
            ("ANTHROPIC_MODEL", self.model.as_str()),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", self.opus_model.as_str()),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL", self.sonnet_model.as_str()),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", self.haiku_model.as_str()),
            ("CLAUDE_CODE_SUBAGENT_MODEL", self.subagent_model.as_str()),
            ("CLAUDE_CODE_EFFORT_LEVEL", self.effort_level.as_str()),
        ] {
            if !value.trim().is_empty() {
                env.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
        }
        Ok(env)
    }
}

pub struct ClaudeProvider {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    claude_home: PathBuf,
    /// 已被服务端作废(`invalid_grant`)的 refresh token 指纹集合。命中即跳过刷新，
    /// 避免 daemon 对死 token 反复刷新打风暴(参见 troubleshooting/2026-06-08)。
    /// 指纹随 token 轮换自动失效——新 token 指纹不同，不在集合内即恢复刷新。
    /// 进程内共享一份即可止住 daemon 长驻保活的风暴；CLI 一次性查询不成风暴，无需持久化。
    dead_refresh: Arc<Mutex<HashSet<String>>>,
}

impl ClaudeProvider {
    pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> Self {
        Self {
            store,
            registry,
            claude_home: claude_home(),
            dead_refresh: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// 指定 refresh token 是否已知作废(指纹命中死集合)。空 token 视为未知(false)。
    fn is_refresh_dead(&self, refresh_token: &str) -> bool {
        if refresh_token.is_empty() {
            return false;
        }
        let fp = refresh_fingerprint(refresh_token);
        self.dead_refresh
            .lock()
            .map(|set| set.contains(&fp))
            .unwrap_or(false)
    }

    /// 把 refresh token 标记为作废。下次刷新前命中即跳过网络请求。
    fn mark_refresh_dead(&self, refresh_token: &str) {
        if refresh_token.is_empty() {
            return;
        }
        let fp = refresh_fingerprint(refresh_token);
        if let Ok(mut set) = self.dead_refresh.lock() {
            set.insert(fp);
        }
    }

    /// 持续回灌:把当前 live(Claude Code 钥匙串/`~/.claude` 文件)凭证抓回它所属账号的 store。
    ///
    /// **只读 live、只写 store、绝不刷新、绝不写 live**，因此对 active 账号也安全——不触碰
    /// 「subswap 不得刷 active / 不得把陈旧 token 写回 live」的红线(troubleshooting/2026-06-08)。
    /// 让当前 active 账号的 store 副本一直贴近 live，等它日后变 parked 时手里仍是活 token，
    /// parked 自刷不再依赖重新打开 Claude Code。daemon 每轮调用一次填补「离开未经 swap」的捕获缺口。
    pub async fn reconcile_active_from_live(&self) -> Result<()> {
        let store = self.store.clone();
        let registry = self.registry.clone();
        let claude_home = self.claude_home.clone();
        tokio::task::spawn_blocking(move || {
            capture_live_into_store(
                store.as_ref(),
                &registry,
                &claude_home,
                prefer_keychain_for_live_capture(),
            )
        })
        .await
        .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))?
    }

    /// 登记一个 Claude Code 自定义 API 配置。只保存，不自动激活。
    pub fn add_api(
        &self,
        id: String,
        label: String,
        api_key: String,
        config: ClaudeApiConfig,
    ) -> Result<Account> {
        validate_api_id(&id)?;
        if api_key.trim().is_empty() {
            return Err(Error::Config("Claude API key cannot be empty".into()));
        }
        if config.base_url.trim().is_empty() {
            return Err(Error::Config("Claude API endpoint cannot be empty".into()));
        }
        // 提前验证认证字段与环境变量结构，避免保存后首次切换才报错。
        config.env(&api_key)?;

        let existing = self.registry.find(PROVIDER_ID, &AccountId(id.clone()))?;
        if existing
            .as_ref()
            .is_some_and(|account| !is_api_account(account))
        {
            return Err(Error::Config(format!(
                "Claude account id {id} is already used by an OAuth account"
            )));
        }
        self.store.set(PROVIDER_ID, &id, API_KEY_FIELD, &api_key)?;
        let mut extra = serde_json::Map::new();
        extra.insert(ACCOUNT_KIND_FIELD.into(), API_KIND.into());
        extra.insert("manual_only".into(), true.into());
        extra.insert(API_CONFIG_FIELD.into(), serde_json::to_value(config)?);
        let account = Account {
            provider: PROVIDER_ID.into(),
            id: AccountId(id),
            label,
            active: existing.as_ref().is_some_and(|account| account.active),
            created_at: existing
                .as_ref()
                .map(|account| account.created_at)
                .unwrap_or_else(Utc::now),
            last_used_at: existing.and_then(|account| account.last_used_at),
            priority: 100,
            extra,
        };
        self.registry.upsert(account.clone())?;
        Ok(account)
    }

    /// 若用户在自定义 API 模式下通过 Claude Code `/login` 外部登录了新账号，
    /// 自动退出 API 模式（恢复 settings.json 并删除 .subswap-api.json 哨兵文件），
    /// 让后续流程正常导入新登录的 OAuth 账号。
    ///
    /// 调和仅在两个用户入口调用（[`Self::import_active`] 和 [`Self::sync_active_metadata`]），
    /// daemon 保活与 quota 自愈等内部路径不触发，避免意外改动状态。
    ///
    /// 操作顺序：先恢复 settings.json，后删除哨兵文件。恢复失败则哨兵保留，下次入口重试。
    fn reconcile_api_external_login(&self) -> Result<()> {
        // 不在 API 模式则无需调和。
        let Some(api_state) = read_api_state(&api_state_path(&self.claude_home))? else {
            return Ok(());
        };
        // 旧版哨兵文件没有记录 oauth_email（account_id 是自定义 API ID 而非 OAuth 邮箱），
        // 无可比较信息，跳过 reconcile 避免误判。
        let Some(saved_email) = &api_state.oauth_email else {
            return Ok(());
        };
        // 无 live oauthAccount（用户已登出），保持 API 模式不动。
        let Some(oauth) = read_oauth_account(&global_config_path(&self.claude_home))? else {
            return Ok(());
        };
        // live oauthAccount 与切入 API 时记录的邮箱一致，无外部登录。
        if &oauth.email_address == saved_email {
            return Ok(());
        }
        tracing::info!(
            api_account = %api_state.account_id,
            oauth_account = %oauth.email_address,
            "检测到 API 模式下外部 Claude Code /login，自动退出 API 模式"
        );
        // 先恢复 settings.json（失败则哨兵保留，下次重试）。
        restore_oauth_env_in_settings(&settings_path(&self.claude_home), &api_state.restore_env)?;
        // 后删除哨兵文件。
        remove_api_state(&api_state_path(&self.claude_home))?;
        Ok(())
    }

    /// 把当前 `~/.claude` 下激活的账号导入为 subswap 管理的账号。
    pub fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
        self.reconcile_api_external_login()?;
        if let Some(account) = self.active_api_account()? {
            return Ok(account);
        }
        let creds = self.read_live_credentials()?;
        let oauth_account = read_oauth_account(&global_config_path(&self.claude_home))?
            .ok_or_else(|| Error::Provider(
                "no oauthAccount in ~/.claude; log into Claude Code first, or use --credentials-file"
                    .into(),
            ))?;
        self.store_account(creds, oauth_account, label_hint)
    }

    /// 仅同步当前 Claude 账号的非敏感元数据,不读写 keyring。
    pub fn sync_active_metadata(&self, label_hint: Option<String>) -> Result<Account> {
        self.reconcile_api_external_login()?;
        if let Some(account) = self.active_api_account()? {
            return Ok(account);
        }
        let oauth_account = read_oauth_account(&global_config_path(&self.claude_home))?
            .ok_or_else(|| Error::Provider(
                "no oauthAccount in ~/.claude; log into Claude Code first, or use --credentials-file"
                    .into(),
            ))?;
        self.upsert_metadata_account(oauth_account, label_hint, Some(true))
    }

    /// 从给定 credentials.json + 可选 oauthAccount 信息导入一个账号。
    pub fn import_from_files(
        &self,
        credentials_json_path: PathBuf,
        oauth_account_path: Option<PathBuf>,
        label_hint: Option<String>,
    ) -> Result<Account> {
        let creds_raw = std::fs::read_to_string(&credentials_json_path)?;
        let creds: CredentialsFile = serde_json::from_str(&creds_raw)?;

        let oauth_account = if let Some(p) = oauth_account_path {
            let raw = std::fs::read_to_string(&p)?;
            serde_json::from_str::<OauthAccount>(&raw)?
        } else {
            let email = label_hint.clone().ok_or_else(|| {
                Error::Provider("without --oauth-account-file you must pass --label <email>".into())
            })?;
            OauthAccount {
                email_address: email,
                account_uuid: None,
                organization_uuid: None,
                organization_name: None,
                other: serde_json::Map::new(),
            }
        };
        self.store_account(creds, oauth_account, label_hint)
    }

    /// 从已解析出的原始 JSON 导入账号。用于迁移其它本地工具的数据。
    pub fn import_from_raw_json(
        &self,
        credentials_json: &str,
        oauth_account_json: &str,
        label_hint: Option<String>,
    ) -> Result<Account> {
        let creds: CredentialsFile = serde_json::from_str(credentials_json)?;
        let oauth_account: OauthAccount = serde_json::from_str(oauth_account_json)?;
        self.store_account(creds, oauth_account, label_hint)
    }

    /// 守护进程后台保活专用:仅当 token 临近过期(`REFRESH_SLACK_MS` 内)才刷新。
    ///
    /// 返回:
    /// - `Ok(true)`  实际触发了刷新
    /// - `Ok(false)` token 还远没过期,或没有 `refresh_token` 不能刷,跳过(非错误)
    /// - `Err(_)`    keyring 读不到该账号,或刷新网络/HTTP 失败
    ///
    /// 不动 `~/.claude/` 下任何文件,只回写 keyring。这是 daemon 周期任务,任一账号失败不影响其它。
    pub async fn refresh_if_near_expiry(&self, id: &AccountId) -> Result<bool> {
        if self
            .registry
            .find(PROVIDER_ID, id)?
            .as_ref()
            .is_some_and(is_api_account)
        {
            return Ok(false);
        }
        // active 账号的 token 由 Claude Code 自己轮换;subswap 在后台刷新只写 keyring、
        // 不写 ~/.claude,会让 live 文件持有的 refresh token 被服务端作废 → "already used"。
        // 因此后台保活只对停泊(parked)账号生效,active 账号直接跳过。
        if self.active_account_id()?.as_ref() == Some(id) {
            return Ok(false);
        }
        let mut creds = self.load_credentials(id)?;
        if !is_expired_or_soon(&creds, settings::current().token.refresh_slack_ms) {
            return Ok(false);
        }
        let refresh_token = creds
            .oauth
            .refresh_token
            .as_deref()
            .unwrap_or("")
            .to_string();
        if refresh_token.is_empty() {
            return Ok(false);
        }
        // 死 token 守卫：已知作废的 refresh token 不再发刷新请求，静默跳过(止住保活风暴)。
        // token 一旦轮换(指纹变化)自动恢复刷新。re-login 的提示由 quota 查询路径透出给用户。
        if self.is_refresh_dead(&refresh_token) {
            return Ok(false);
        }
        match apply_refresh_to_creds(&mut creds).await {
            Ok(()) => {
                self.save_credentials(id, &creds)?;
                Ok(true)
            }
            Err(e) => {
                // 仅 invalid_grant 才判死;网络/超时等瞬时错误下轮照常重试。
                if is_invalid_grant(&e) {
                    self.mark_refresh_dead(&refresh_token);
                }
                Err(e)
            }
        }
    }

    /// 显式刷新指定账号的 access_token，并写回 keyring。
    ///
    /// 与 [`Self::activate`] 内的「best-effort 预刷新」不同：
    /// - 这里**无条件**刷新（不管 expiresAt）
    /// - 失败直接返回 Err（用户主动调用，需要明确反馈）
    /// - 不动 ~/.claude/ 下任何文件（可能该账号不是当前激活的）
    pub async fn refresh_account(&self, id: &AccountId) -> Result<()> {
        let mut creds = self.load_credentials(id)?;
        apply_refresh_to_creds(&mut creds).await?;
        self.save_credentials(id, &creds)?;
        Ok(())
    }

    /// 读取当前激活账号的实时凭证。
    ///
    /// macOS 上 Claude Code 的真实 live 凭证在钥匙串，`.credentials.json` 可能只是 subswap
    /// 上次切换留下的陈旧副本，因此必须优先读钥匙串。
    fn read_live_credentials(&self) -> Result<CredentialsFile> {
        #[cfg(target_os = "macos")]
        if let Some(creds) = read_claude_code_keychain() {
            return Ok(creds);
        }
        read_credentials(&credentials_path(&self.claude_home))
    }

    /// 当前 Claude Code 激活账号的 id(取自 `~/.claude.json` 的 `oauthAccount`)。不读 keyring。
    /// 没有 oauthAccount(未登录)时返回 `None`。daemon 保活与 quota 自愈用它跳过 active 账号——
    /// active 账号的 token 由 Claude Code 唯一轮换,subswap 不得在后台抢刷。
    fn active_account_id(&self) -> Result<Option<AccountId>> {
        if let Some(state) = read_api_state(&api_state_path(&self.claude_home))? {
            return Ok(Some(AccountId(state.account_id)));
        }
        let Some(oauth_account) = read_oauth_account(&global_config_path(&self.claude_home))?
        else {
            return Ok(None);
        };
        Ok(Some(AccountId(oauth_account.email_address)))
    }

    /// 从凭证仓库读账号的 credentials JSON 副本。
    /// 仓库缺失时(典型：macOS 首次,凭证只在 Claude Code 钥匙串里),对**当前激活账号**做一次性捕获:
    /// 读 Claude Code 钥匙串 → 落盘进仓库,之后走仓库(FileStore 明文)不再碰钥匙串。
    fn load_credentials(&self, id: &AccountId) -> Result<CredentialsFile> {
        if let Some(raw) = self.store.get(PROVIDER_ID, id.0.as_str(), CRED_FIELD)? {
            return Ok(serde_json::from_str(&raw)?);
        }
        if let Some(creds) = self.capture_from_claude_code_keychain(id)? {
            return Ok(creds);
        }
        Err(Error::Credential(format!(
            "no credentials for {PROVIDER_ID}:{id}; run `subswap login claude` (or swap to this account first)"
        )))
    }

    /// 当 `id` 是 Claude Code 当前激活账号时,把它钥匙串里的凭证捕获进仓库。返回捕获到的凭证。
    /// 非激活账号(钥匙串里的凭证不属于它)或读不到时返回 `None`,由调用方报「缺凭证」。
    fn capture_from_claude_code_keychain(&self, id: &AccountId) -> Result<Option<CredentialsFile>> {
        // 钥匙串 item 只保存「当前激活」那个账号的凭证;用 ~/.claude.json 的 oauthAccount 判断归属。
        let Some(oauth_account) = read_oauth_account(&global_config_path(&self.claude_home))?
        else {
            return Ok(None);
        };
        if oauth_account.email_address != id.0 {
            return Ok(None);
        }
        let Some(creds) = read_claude_code_keychain() else {
            return Ok(None);
        };
        // 落盘,后续查询走仓库、不再读钥匙串。
        self.save_credentials(id, &creds)?;
        tracing::info!(account=%id, "captured Claude credentials from Claude Code keychain into store");
        Ok(Some(creds))
    }

    /// 把 [`CredentialsFile`] 写回 keyring。
    fn save_credentials(&self, id: &AccountId, creds: &CredentialsFile) -> Result<()> {
        let serialized = serde_json::to_string(creds)?;
        self.store
            .set(PROVIDER_ID, id.0.as_str(), CRED_FIELD, &serialized)
    }

    fn active_api_account(&self) -> Result<Option<Account>> {
        let Some(state) = read_api_state(&api_state_path(&self.claude_home))? else {
            return Ok(None);
        };
        let id = AccountId(state.account_id);
        let account = self.registry.find(PROVIDER_ID, &id)?.ok_or_else(|| {
            Error::Provider(format!(
                "active Claude API marker references missing account {PROVIDER_ID}:{id}"
            ))
        })?;
        if !is_api_account(&account) {
            return Err(Error::Provider(format!(
                "active Claude API marker references non-API account {PROVIDER_ID}:{id}"
            )));
        }
        Ok(Some(account))
    }

    async fn activate_api(&self, account: Account) -> Result<()> {
        let id = account.id.clone();
        let config = api_config(&account)?;
        let api_key = self
            .store
            .get(PROVIDER_ID, &id.0, API_KEY_FIELD)?
            .ok_or_else(|| {
                Error::Credential(format!(
                    "no API key for {PROVIDER_ID}:{id}; re-add it with `subswap add-api`"
                ))
            })?;
        let api_env = config.env(&api_key)?;
        let settings_path = settings_path(&self.claude_home);
        let state_path = api_state_path(&self.claude_home);
        let existing_state = read_api_state(&state_path)?;
        let restore_env = match &existing_state {
            Some(state) => state.restore_env.clone(),
            None => capture_managed_env(&read_settings(&settings_path)?),
        };
        // 保留已知的 oauth_email（重复切入同一 API 账号时不丢失），首次切入则捕获当前值。
        let oauth_email = existing_state.and_then(|s| s.oauth_email).or_else(|| {
            read_oauth_account(&global_config_path(&self.claude_home))
                .ok()
                .flatten()
                .map(|a| a.email_address)
        });
        let state = ApiState {
            account_id: id.0.clone(),
            oauth_email,
            restore_env,
        };
        let registry = self.registry.clone();
        let claude_home = self.claude_home.clone();
        tokio::task::spawn_blocking(move || {
            let targets = vec![
                SwapTarget {
                    snapshot_name: "settings.json",
                    live_path: settings_path,
                    writer: Box::new(move |path| write_api_env_into_settings(path, &api_env)),
                },
                SwapTarget {
                    snapshot_name: "api-state.json",
                    live_path: state_path,
                    writer: Box::new(move |path| write_api_state(path, &state)),
                },
            ];
            swap_with_snapshot(PROVIDER_ID, &claude_home, targets, || {
                registry.set_active(PROVIDER_ID, &id)
            })
        })
        .await
        .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))?
    }

    /// 公共入库逻辑：写 keyring + 写 registry.toml。
    ///
    /// 语义说明：对已存在的账号执行重新导入时，**保留** `active` 标记不变。理由：
    /// 防止误操作（比如用临时 token 覆盖一个正在使用的账号）连带改变激活账号。
    /// 若用户希望同时激活，请显式调用 `subswap swap`。
    fn store_account(
        &self,
        creds: CredentialsFile,
        oauth_account: OauthAccount,
        label_hint: Option<String>,
    ) -> Result<Account> {
        let id = AccountId(oauth_account.email_address.clone());
        let label = label_hint.unwrap_or_else(|| oauth_account.email_address.clone());

        let creds_json = serde_json::to_string(&creds)?;
        self.store
            .set(PROVIDER_ID, id.0.as_str(), CRED_FIELD, &creds_json)?;

        let mut extra = serde_json::Map::new();
        extra.insert(
            "oauth_account".into(),
            serde_json::to_value(&oauth_account)?,
        );

        let existing = self.registry.find(PROVIDER_ID, &id)?;
        let account = Account {
            provider: PROVIDER_ID.into(),
            id: id.clone(),
            label,
            active: existing.as_ref().is_some_and(|a| a.active),
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

    fn upsert_metadata_account(
        &self,
        oauth_account: OauthAccount,
        label_hint: Option<String>,
        active_override: Option<bool>,
    ) -> Result<Account> {
        let id = AccountId(oauth_account.email_address.clone());
        let label = label_hint.unwrap_or_else(|| oauth_account.email_address.clone());

        let mut extra = serde_json::Map::new();
        extra.insert(
            "oauth_account".into(),
            serde_json::to_value(&oauth_account)?,
        );

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
}

#[async_trait]
impl Provider for ClaudeProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "Claude / Anthropic"
    }

    fn client_targets(&self) -> Vec<ClientTarget> {
        vec![
            ClientTarget {
                id: "claude_credentials".into(),
                display_name: "Claude credentials".into(),
                probe_path: credentials_path(&self.claude_home),
            },
            ClientTarget {
                id: "claude_global_config".into(),
                display_name: "Claude global config".into(),
                probe_path: global_config_path(&self.claude_home),
            },
            ClientTarget {
                id: "claude_settings".into(),
                display_name: "Claude settings".into(),
                probe_path: settings_path(&self.claude_home),
            },
        ]
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(PROVIDER_ID)
    }

    async fn activate(&self, id: &AccountId) -> Result<()> {
        // ----- 阶段 1：异步预处理（拉取目标账号、best-effort 刷新 token） -----
        let account =
            self.registry
                .find(PROVIDER_ID, id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                })?;
        if is_api_account(&account) {
            return self.activate_api(account).await;
        }
        // 重复切换当前账号时绝不能从 store 把陈旧 token 写回 live。Claude Code 的 /login 或
        // 自动轮换可能已经更新钥匙串；此时只回灌最新 live 凭证并保持现状。
        if self.active_account_id()?.as_ref() == Some(id) {
            let claude_home = self.claude_home.clone();
            let registry = self.registry.clone();
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = capture_live_into_store(
                    store.as_ref(),
                    &registry,
                    &claude_home,
                    prefer_keychain_for_live_capture(),
                ) {
                    tracing::warn!(err = %e, "claude active-account capture failed; leaving live credentials unchanged");
                }
            })
            .await
            .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))?;
            tracing::info!(account = %id, "Claude account already active; live credentials preserved");
            return Ok(());
        }
        let mut target_creds = self.load_credentials(id)?;
        let target_oauth_account: OauthAccount = serde_json::from_value(
            account.extra.get("oauth_account").cloned().ok_or_else(|| {
                Error::Provider(format!(
                    "registry entry {PROVIDER_ID}:{id} missing oauth_account field"
                ))
            })?,
        )?;

        // best-effort 预刷新：失败不阻塞 activate，保持「网络挂了也能切」的不变量。
        if best_effort_pre_refresh(&mut target_creds).await {
            // 回写 keyring，避免下次 activate 重复刷。Best-effort：失败仅日志。
            if let Err(e) = self.save_credentials(id, &target_creds) {
                tracing::warn!(err=%e, "writing refreshed token back to keyring failed; next activate will refresh again");
            }
        }

        // ----- 阶段 2：同步阻塞部分（flock + 文件 IO + registry 更新） -----
        let creds_path = credentials_path(&self.claude_home);
        let conf_path = global_config_path(&self.claude_home);
        let settings_path = settings_path(&self.claude_home);
        let state_path = api_state_path(&self.claude_home);
        let api_state = read_api_state(&state_path)?;
        let claude_home = self.claude_home.clone();
        let registry = self.registry.clone();
        let store = self.store.clone();
        let id_for_blocking = id.clone();

        tokio::task::spawn_blocking(move || {
            // capture-on-leave：覆盖 live 文件前，把当前 live 凭证回灌进它所属账号的 store。
            // 否则切走的账号 store 副本会停在旧 refresh token，下次切回写回旧 token → "already used"。
            if let Err(e) = capture_live_into_store(
                store.as_ref(),
                &registry,
                &claude_home,
                prefer_keychain_for_live_capture(),
            ) {
                tracing::warn!(err = %e, "claude capture-on-leave failed; continuing swap");
            }

            // macOS 上 Claude Code 只认自己的 Keychain item；仅写 `.credentials.json`
            // 会导致列表显示已切换，但 Claude Code 启动后仍恢复成旧账号。
            #[cfg(target_os = "macos")]
            let keychain_backup = snapshot_claude_code_keychain()?;
            #[cfg(target_os = "macos")]
            write_claude_code_keychain(&target_creds)?;

            let mut targets = vec![
                SwapTarget {
                    snapshot_name: "credentials.json",
                    live_path: creds_path,
                    writer: Box::new(move |p| write_credentials(p, &target_creds)),
                },
                SwapTarget {
                    snapshot_name: "config.json",
                    live_path: conf_path,
                    writer: Box::new(move |p| {
                        write_oauth_account_into_global(p, &target_oauth_account)
                    }),
                },
            ];
            if let Some(state) = api_state {
                targets.push(SwapTarget {
                    snapshot_name: "settings.json",
                    live_path: settings_path,
                    writer: Box::new(move |path| {
                        restore_oauth_env_in_settings(path, &state.restore_env)
                    }),
                });
                targets.push(SwapTarget {
                    snapshot_name: "api-state.json",
                    live_path: state_path,
                    writer: Box::new(remove_api_state),
                });
            }
            let result = swap_with_snapshot(PROVIDER_ID, &claude_home, targets, || {
                registry.set_active(PROVIDER_ID, &id_for_blocking)
            });
            #[cfg(target_os = "macos")]
            if result.is_err() {
                if let Err(e) = restore_claude_code_keychain(keychain_backup) {
                    tracing::error!(err = %e, "Claude Code keychain rollback failed");
                }
            }
            if result.is_ok() {
                tracing::info!(account = %id_for_blocking, "Claude swap done");
            }
            result
        })
        .await
        .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))?
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        if self
            .registry
            .find(PROVIDER_ID, id)?
            .as_ref()
            .is_some_and(is_api_account)
        {
            return Ok(Vec::new());
        }
        let (mut creds, from_live) = match self.read_active_credentials_if_matches(id)? {
            // 命中本地实体文件(~/.claude/.credentials.json)→ 这是 active 账号,Claude Code 持有它。
            Some(creds) => (creds, true),
            // 实体文件缺失/不匹配时回落凭证仓库(parked 账号)。macOS 上 Claude Code 把凭证存进钥匙串、
            // 不写实体文件,激活账号也走这里;FileStore 后端是明文文件,读任何账号都不弹钥匙串。
            None => (self.load_credentials(id)?, false),
        };
        // 进程内自愈：access_token 失效(401)且有 refresh_token 时，best-effort 刷新一次再重试。
        // 动机：daemon 后台保活在部分环境(如 Linux keyutils 按 session 隔离)读不到本进程写入的
        //       keyring 条目，无法保活；查询进程能看到自己的 keyring，因此在这里自愈最可靠。
        // 关键约束：仅对 parked 账号自愈刷新。active 账号(from_live)的 token 由 Claude Code 唯一
        // 轮换,subswap 刷新只写 keyring、不写 live 文件,会让 live 持有的 refresh token 被作废 →
        // "refresh token already used"。保守起见只在 401 时刷新、且只重试一次,避免请求风暴(AGENTS.md #10)。
        let usage = match oauth::fetch_usage(&creds.oauth.access_token).await {
            Ok(u) => u,
            Err(e) if is_auth_error(&e) && !from_live && creds.oauth.refresh_token.is_some() => {
                let refresh_token = creds.oauth.refresh_token.clone().unwrap_or_default();
                // 死 token 守卫：refresh token 已知作废时不再尝试刷新，直接透出 re-login。
                if self.is_refresh_dead(&refresh_token) {
                    return Err(relogin_required_error(id));
                }
                match apply_refresh_to_creds(&mut creds).await {
                    Ok(()) => {
                        // 刷新后的 token 写回凭证仓库,避免下次查询重复刷新(FileStore 写入不弹钥匙串)。
                        self.save_credentials(id, &creds)?;
                    }
                    Err(re) if is_invalid_grant(&re) => {
                        self.mark_refresh_dead(&refresh_token);
                        return Err(relogin_required_error(id));
                    }
                    Err(re) => return Err(re),
                }
                oauth::fetch_usage(&creds.oauth.access_token).await?
            }
            Err(e) => return Err(e),
        };

        let mut out = Vec::new();
        if let Some(five) = usage.five_hour {
            out.push(make_quota(
                id,
                QuotaWindow::FiveHour,
                five.utilization,
                five.resets_at,
            ));
        }
        if let Some(seven) = usage.seven_day {
            out.push(make_quota(
                id,
                QuotaWindow::SevenDay,
                seven.utilization,
                seven.resets_at,
            ));
        }
        if let Some(extra) = usage.extra_usage {
            out.push(make_quota(
                id,
                QuotaWindow::Month,
                extra.utilization,
                extra.resets_at,
            ));
        }
        Ok(out)
    }
}

/// macOS：隔离环境下 Claude Code 凭证钥匙串 item 的 service 名。
///
/// 公式见 docs/design/ACCOUNT_ISOLATION_DESIGN.md §2.1：
/// `Claude Code-credentials-<sha256(NFC(securestorage_dir)).hex[:8]>`。
/// subswap 启动隔离子进程时显式设 `CLAUDE_SECURESTORAGE_CONFIG_DIR=<dir>`，使哈希源为我们已知的
/// 确切字符串，不依赖 claude 内部对 `CLAUDE_CONFIG_DIR` 的解析（规避 service 名推导漂移）。
/// 路径通常为 ASCII，NFC 规范化对其恒等；非 ASCII 路径暂按原始 UTF-8 字节处理。
#[cfg(target_os = "macos")]
pub fn isolated_keychain_service(securestorage_dir: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(securestorage_dir.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{CLAUDE_CODE_KEYCHAIN_SERVICE}-{}", &hex[..8])
}

impl ClaudeProvider {
    /// API 账号专用：返回进程级隔离所需的 env vars（直接注入子进程，无需物化目录）。
    /// OAuth 账号返回 None，调用方走常规 materialize 路径。
    pub fn api_run_env_vars(&self, id: &AccountId) -> Result<Option<Vec<(String, String)>>> {
        let account =
            self.registry
                .find(PROVIDER_ID, id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                })?;
        if !is_api_account(&account) {
            return Ok(None);
        }
        let api_key = self
            .store
            .get(PROVIDER_ID, &id.0, API_KEY_FIELD)?
            .ok_or_else(|| {
                Error::Credential(format!(
                    "no API key for {PROVIDER_ID}:{id}; re-add it with `subswap add-api`"
                ))
            })?;
        let config = api_config(&account)?;
        let map = config.env(&api_key)?;
        let vars = map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect();
        Ok(Some(vars))
    }

    /// 取账号用于隔离环境的 OAuth 凭证 + oauthAccount 元数据。
    /// active 账号读 live（macOS 钥匙串 / 文件），parked 账号读凭证仓库。自定义 API 账号不支持隔离。
    pub fn export_isolated_credentials(
        &self,
        id: &AccountId,
    ) -> Result<(CredentialsFile, OauthAccount)> {
        let account =
            self.registry
                .find(PROVIDER_ID, id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                })?;
        if is_api_account(&account) {
            return Err(Error::Provider(
                "Claude custom-API accounts cannot be isolated via `run`; they use settings.json env"
                    .into(),
            ));
        }
        let creds = if self.active_account_id()?.as_ref() == Some(id) {
            self.read_live_credentials()?
        } else {
            self.load_credentials(id)?
        };
        let oauth_account: OauthAccount = serde_json::from_value(
            account.extra.get("oauth_account").cloned().ok_or_else(|| {
                Error::Provider(format!(
                    "registry entry {PROVIDER_ID}:{id} missing oauth_account field"
                ))
            })?,
        )?;
        Ok((creds, oauth_account))
    }

    /// 把账号凭证物化进隔离 config 目录：写 `.credentials.json` + `.claude.json`(oauthAccount)；
    /// macOS 额外写命名空间钥匙串 item —— Claude Code 在 macOS 实际从钥匙串读凭证。
    /// 除账号文件外，其余 Claude home 内容链接回全局目录，让 `subswap run` 只改变账号身份，
    /// 不改变 settings / skills / plugins / projects / session 历史等工作环境。
    pub fn materialize_isolated(&self, id: &AccountId, config_dir: &Path) -> Result<()> {
        let (creds, oauth_account) = self.export_isolated_credentials(id)?;
        std::fs::create_dir_all(config_dir)?;
        link_shared_claude_home(&self.claude_home, config_dir)?;
        write_credentials(&credentials_path(config_dir), &creds)?;
        write_oauth_account_into_global(&global_config_path(config_dir), &oauth_account)?;
        // 预置引导完成标记，使 claude 跳过「Select login method」首次引导流程。
        // claude 在 hasCompletedOnboarding 缺失时强制跑引导，即使钥匙串已有有效凭证也一样。
        // 详见 docs/design/ACCOUNT_ISOLATION_DESIGN.md §2.3。
        mark_onboarding_complete(&global_config_path(config_dir))?;
        #[cfg(target_os = "macos")]
        {
            let service = isolated_keychain_service(&config_dir.to_string_lossy());
            security_set_password_for(&service, &serde_json::to_string(&creds)?)?;
        }
        Ok(())
    }

    /// 隔离会话结束后把（可能被 Claude Code 轮换过的）凭证吸收回凭证仓库，不动全局 active。
    pub fn absorb_isolated(&self, id: &AccountId, config_dir: &Path) -> Result<()> {
        match self.read_isolated_credentials(config_dir) {
            Some(creds) => {
                self.save_credentials(id, &creds)?;
                tracing::info!(account = %id, "absorbed claude credentials from isolated session");
                Ok(())
            }
            None => Err(Error::Provider(format!(
                "no credentials found in isolated env {}",
                config_dir.display()
            ))),
        }
    }

    /// macOS：优先读命名空间钥匙串，回落 `.credentials.json`。
    #[cfg(target_os = "macos")]
    fn read_isolated_credentials(&self, config_dir: &Path) -> Option<CredentialsFile> {
        let service = isolated_keychain_service(&config_dir.to_string_lossy());
        if let Ok(Some(raw)) = security_find_password_for(&service) {
            if let Ok(creds) = serde_json::from_str::<CredentialsFile>(&raw) {
                return Some(creds);
            }
        }
        read_credentials(&credentials_path(config_dir)).ok()
    }

    /// 非 macOS：凭证只在 `.credentials.json`。
    #[cfg(not(target_os = "macos"))]
    fn read_isolated_credentials(&self, config_dir: &Path) -> Option<CredentialsFile> {
        read_credentials(&credentials_path(config_dir)).ok()
    }
}

/// 让隔离 Claude config 目录共享全局 Claude home 中除账号文件外的所有内容。
///
/// Claude Code 把账号凭据和 oauthAccount 也放在 config dir 内，所以这些账号相关文件必须
/// 留在隔离目录；其它配置、skills、plugins、projects 等都应共享全局，避免隔离账号跑出
/// 另一套工作环境。
fn link_shared_claude_home(global_home: &Path, isolated_home: &Path) -> Result<()> {
    std::fs::create_dir_all(global_home)?;
    std::fs::create_dir_all(isolated_home)?;

    let mut names = BTreeSet::new();
    for entry in std::fs::read_dir(global_home)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if should_share_claude_entry(&name) {
            names.insert(name);
        }
    }
    for name in essential_shared_claude_dirs() {
        names.insert((*name).to_string());
    }

    for name in names {
        let target = global_home.join(&name);
        if !target.exists() {
            if is_essential_shared_dir(name.as_str()) {
                std::fs::create_dir_all(&target)?;
            } else {
                continue;
            }
        }
        let link_path = isolated_home.join(&name);
        replace_with_shared_path(&link_path, &target)?;
    }

    copy_shared_settings_without_account_env(global_home, isolated_home, "settings.json")?;
    copy_shared_settings_without_account_env(global_home, isolated_home, "settings.local.json")?;
    Ok(())
}

fn essential_shared_claude_dirs() -> &'static [&'static str] {
    &[
        "agents",
        "commands",
        "file-history",
        "hooks",
        "ide",
        "memory",
        "plugins",
        "projects",
        "skills",
        "statsig",
        "todos",
    ]
}

fn is_essential_shared_dir(name: &str) -> bool {
    essential_shared_claude_dirs().contains(&name)
}

fn should_share_claude_entry(name: &str) -> bool {
    !matches!(
        name,
        ".credentials.json" | ".claude.json" | ".config.json" | ".subswap-api.json"
    ) && !matches!(name, "settings.json" | "settings.local.json")
        && !name.starts_with(".subswap.")
}

fn copy_shared_settings_without_account_env(
    global_home: &Path,
    isolated_home: &Path,
    file_name: &str,
) -> Result<()> {
    let src = global_home.join(file_name);
    if !src.exists() {
        return Ok(());
    }
    let mut settings = read_settings(&src)?;
    strip_managed_account_env(&mut settings)?;
    let dst = isolated_home.join(file_name);
    if let Ok(meta) = std::fs::symlink_metadata(&dst) {
        if meta.file_type().is_symlink() || meta.is_file() {
            std::fs::remove_file(&dst)?;
        } else if meta.is_dir() {
            std::fs::remove_dir_all(&dst)?;
        }
    }
    std::fs::write(&dst, serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}

fn strip_managed_account_env(settings: &mut serde_json::Value) -> Result<()> {
    let obj = settings
        .as_object_mut()
        .ok_or_else(|| Error::Provider("Claude settings root is not a JSON object".into()))?;
    let remove_env = if let Some(env) = obj.get_mut("env") {
        let env = env
            .as_object_mut()
            .ok_or_else(|| Error::Provider("Claude settings env is not a JSON object".into()))?;
        for key in MANAGED_API_ENV_KEYS {
            env.remove(*key);
        }
        env.is_empty()
    } else {
        false
    };
    if remove_env {
        obj.remove("env");
    }
    Ok(())
}

fn replace_with_shared_path(link_path: &Path, target: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(link_path) {
        if meta.file_type().is_symlink() {
            std::fs::remove_file(link_path)?;
        } else if meta.is_dir() {
            if target.is_dir() {
                merge_dir_contents(link_path, target)?;
            }
            std::fs::remove_dir_all(link_path)?;
        } else {
            std::fs::remove_file(link_path)?;
        }
    }
    create_link(target, link_path)
}

fn merge_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let meta = entry.metadata()?;
        if meta.is_dir() {
            merge_dir_contents(&src_path, &dst_path)?;
        } else if !dst_path.exists() {
            if let Some(parent) = dst_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_link(target: &Path, link_path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link_path)?;
    Ok(())
}

#[cfg(windows)]
fn create_link(target: &Path, link_path: &Path) -> Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link_path)?;
    } else {
        std::os::windows::fs::symlink_file(target, link_path)?;
    }
    Ok(())
}

fn is_api_account(account: &Account) -> bool {
    account
        .extra
        .get(ACCOUNT_KIND_FIELD)
        .and_then(serde_json::Value::as_str)
        == Some(API_KIND)
}

fn api_config(account: &Account) -> Result<ClaudeApiConfig> {
    serde_json::from_value(
        account
            .extra
            .get(API_CONFIG_FIELD)
            .cloned()
            .ok_or_else(|| {
                Error::Provider(format!(
                    "registry entry {PROVIDER_ID}:{} missing api_config field",
                    account.id
                ))
            })?,
    )
    .map_err(Error::from)
}

fn validate_api_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.contains('/')
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::Config(
            "Claude API id must use only letters, numbers, '-', '_' or '.'".into(),
        ));
    }
    Ok(())
}

impl ClaudeProvider {
    fn read_active_credentials_if_matches(
        &self,
        id: &AccountId,
    ) -> Result<Option<CredentialsFile>> {
        let Some(oauth_account) = read_oauth_account(&global_config_path(&self.claude_home))?
        else {
            return Ok(None);
        };
        if oauth_account.email_address == id.0 {
            return self.read_live_credentials().map(Some);
        }
        Ok(None)
    }
}

/// macOS：Claude Code 凭证所在 Keychain generic password 的 service 名。
#[cfg(target_os = "macos")]
const CLAUDE_CODE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// macOS：取当前登录用户名，作为 Keychain item 的 account 维度。
#[cfg(target_os = "macos")]
fn keychain_account() -> Result<String> {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            Error::Credential("USER is empty; cannot access Claude Code keychain".into())
        })
}

/// macOS：统一通过 `/usr/bin/security` 命令行访问 Keychain。
///
/// **为什么必须走 CLI 而不是 `keyring` crate**：Keychain item 的 ACL 只信任「创建它的应用」。
/// 用 `keyring`(security-framework 原生 API)写,会把 ACL 设成「仅 subswap 本体」;而 Claude Code
/// 自己是 fork `/usr/bin/security` 来读凭证的,读取方与创建方不一致 → 系统每次读取都弹授权框
/// （`security wants to access "Claude Code-credentials"`)。改为同样用 `/usr/bin/security` 读写后,
/// 创建方与 Claude Code 的读取方都是 `security` 本体,ACL 天然一致,从根上消除反复弹窗。
#[cfg(target_os = "macos")]
fn run_security(args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("/usr/bin/security")
        .args(args)
        .output()
        .map_err(|e| Error::Credential(format!("run /usr/bin/security failed: {e}")))
}

/// 测试隔离用：指定一个显式 keychain 文件路径，让所有 `security` 子命令只作用于它，
/// 不碰用户真实的登录钥匙串。生产环境不设此变量，沿用 `<default>` 登录钥匙串。
///
/// 集成测试若不重定向，会真实弹 macOS 授权框并改写用户登录钥匙串——既污染本机凭证，
/// 也让 CI / 本地 `cargo test` 卡在交互弹窗上。
#[cfg(target_os = "macos")]
fn claude_keychain_override() -> Option<String> {
    std::env::var("SUBSWAP_CLAUDE_KEYCHAIN_PATH")
        .ok()
        .filter(|p| !p.is_empty())
}

/// 在一组 `security` 基础参数后追加显式 keychain 路径（若设置了重定向）。
#[cfg(target_os = "macos")]
fn run_security_on_keychain(base: &[&str]) -> Result<std::process::Output> {
    match claude_keychain_override() {
        Some(path) => {
            let mut args: Vec<&str> = base.to_vec();
            args.push(path.as_str());
            run_security(&args)
        }
        None => run_security(base),
    }
}

/// macOS：读 Claude Code 的系统钥匙串 generic password —— `service = "Claude Code-credentials"`,
/// `account = <登录用户名>`,内容与 `.credentials.json` 同构(`{"claudeAiOauth": {...}}`)。
/// 这是 macOS 上 claude 凭证的唯一来源。读不到(不存在 / 用户拒绝授权 / 解析失败)一律返回 `None`。
#[cfg(target_os = "macos")]
fn read_claude_code_keychain() -> Option<CredentialsFile> {
    let raw = security_find_password().ok().flatten()?;
    serde_json::from_str::<CredentialsFile>(&raw).ok()
}

/// macOS：读出 Keychain item 的明文（找不到返回 `Ok(None)`,执行失败返回 `Err`）。
#[cfg(target_os = "macos")]
fn security_find_password() -> Result<Option<String>> {
    security_find_password_for(CLAUDE_CODE_KEYCHAIN_SERVICE)
}

/// macOS：按指定 service 读 Keychain item 明文。隔离环境用命名空间 service（见
/// [`isolated_keychain_service`]）；全局账号用 [`CLAUDE_CODE_KEYCHAIN_SERVICE`]。
#[cfg(target_os = "macos")]
fn security_find_password_for(service: &str) -> Result<Option<String>> {
    let account = keychain_account()?;
    let output =
        run_security_on_keychain(&["find-generic-password", "-s", service, "-a", &account, "-w"])?;
    if !output.status.success() {
        // 退出码 44 = item 不存在;其余失败(含用户拒绝授权)也按读不到处理。
        return Ok(None);
    }
    let mut raw = String::from_utf8(output.stdout)
        .map_err(|e| Error::Credential(format!("Claude Code keychain non-UTF8: {e}")))?;
    if raw.ends_with('\n') {
        raw.pop();
    }
    Ok(Some(raw))
}

/// 在写入目标账号前保存 Claude Code Keychain 原值，供后续事务失败时恢复。
#[cfg(target_os = "macos")]
fn snapshot_claude_code_keychain() -> Result<Option<String>> {
    security_find_password()
}

/// 把目标账号凭证写入 Claude Code 在 macOS 上真正读取的 Keychain item。
#[cfg(target_os = "macos")]
fn write_claude_code_keychain(creds: &CredentialsFile) -> Result<()> {
    let raw = serde_json::to_string(creds)?;
    security_set_password(&raw)
}

/// macOS：通过 `/usr/bin/security` 写 Keychain item。
///
/// 先 `-U` 原地更新;若失败(item 不存在,或 ACL 被旧 `keyring` 写法污染成「仅 subswap」无法更新),
/// 则删除后用 `security` 重建,使创建方重新变回 `security` 本体、ACL 复位为与 Claude Code 一致。
/// 重建路径首次会对被污染的旧 item 弹一次授权框,之后稳态不再弹。
#[cfg(target_os = "macos")]
fn security_set_password(value: &str) -> Result<()> {
    security_set_password_for(CLAUDE_CODE_KEYCHAIN_SERVICE, value)
}

/// macOS：按指定 service 写 Keychain item，沿用 `-U` 更新 → 删除重建的 ACL 复位策略。
#[cfg(target_os = "macos")]
fn security_set_password_for(service: &str, value: &str) -> Result<()> {
    let account = keychain_account()?;
    let update = run_security_on_keychain(&[
        "add-generic-password",
        "-U",
        "-s",
        service,
        "-a",
        &account,
        "-w",
        value,
    ])?;
    if update.status.success() {
        return Ok(());
    }
    // 删除旧 item(忽略「不存在」类失败),再以 security 为创建者重建。
    let _ = run_security_on_keychain(&["delete-generic-password", "-s", service, "-a", &account])?;
    let add = run_security_on_keychain(&[
        "add-generic-password",
        "-s",
        service,
        "-a",
        &account,
        "-w",
        value,
    ])?;
    if add.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&add.stderr);
    Err(Error::Credential(format!(
        "write Claude Code keychain failed: {stderr}"
    )))
}

#[cfg(target_os = "macos")]
fn restore_claude_code_keychain(backup: Option<String>) -> Result<()> {
    match backup {
        Some(raw) => security_set_password(&raw),
        None => {
            let account = keychain_account()?;
            // 回滚到「原本无 item」状态:删除即可,忽略「不存在」类失败。
            let _ = run_security_on_keychain(&[
                "delete-generic-password",
                "-s",
                CLAUDE_CODE_KEYCHAIN_SERVICE,
                "-a",
                &account,
            ])?;
            Ok(())
        }
    }
}

/// 非 macOS：凭证走实体文件,无此回落。
#[cfg(not(target_os = "macos"))]
fn read_claude_code_keychain() -> Option<CredentialsFile> {
    None
}

#[cfg(target_os = "macos")]
fn prefer_keychain_for_live_capture() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
fn prefer_keychain_for_live_capture() -> bool {
    false
}

/// 在覆盖 live 文件之前，把当前 live 凭证回灌进它所属账号的 store。
///
/// 动机：Claude Code 在使用期间会轮换 refresh token，store 里的冻结快照会逐渐落后；
/// 若不回灌，下次 swap 回该账号会写回旧 token，导致 "refresh token already used"。
/// best-effort：读不到 live 凭证、没有 oauthAccount、或该账号未受管时直接跳过(返回 `Ok`)。
fn capture_live_into_store(
    store: &dyn CredentialStore,
    registry: &AccountRegistry,
    claude_home: &Path,
    prefer_keychain: bool,
) -> Result<()> {
    // macOS 的真实 live 凭证在 Claude Code Keychain；`.credentials.json` 可能是上次
    // subswap 切换残留的 stale 副本，若优先读文件会把错误凭证回灌给当前账号。
    let file_creds = || read_credentials(&credentials_path(claude_home)).ok();
    let live_creds = if prefer_keychain {
        read_claude_code_keychain()
    } else {
        file_creds().or_else(read_claude_code_keychain)
    };
    let Some(live_creds) = live_creds else {
        return Ok(());
    };
    // 归属判定:~/.claude.json 的 oauthAccount.emailAddress。
    let Some(oauth_account) = read_oauth_account(&global_config_path(claude_home))? else {
        return Ok(());
    };
    let id = AccountId(oauth_account.email_address);
    // 仅当该账号确实受 subswap 管理时才回灌(直接登录的临时账号不碰)。
    if registry.find(PROVIDER_ID, &id)?.is_none() {
        return Ok(());
    }

    // 守卫：live 读到的 refresh token 为空时，不能让它覆盖 store 里已有的非空 refresh token。
    // Claude Code 轮换 token 期间钥匙串可能短暂处于「有 access、缺 refresh」的状态;若照单全收,
    // store 里原本可续期的账号会被写成永久过期(active 账号只读不刷,这个空缺再也补不回来)。
    // 命中时只跟进 access token / expiresAt,保留旧 refresh token。
    if live_creds
        .oauth
        .refresh_token
        .as_deref()
        .unwrap_or("")
        .is_empty()
    {
        if let Some(raw) = store.get(PROVIDER_ID, id.0.as_str(), CRED_FIELD)? {
            if let Ok(mut existing) = serde_json::from_str::<CredentialsFile>(&raw) {
                if !existing
                    .oauth
                    .refresh_token
                    .as_deref()
                    .unwrap_or("")
                    .is_empty()
                {
                    existing.oauth.access_token = live_creds.oauth.access_token;
                    existing.oauth.expires_at = live_creds.oauth.expires_at;
                    let merged = serde_json::to_string(&existing)?;
                    store.set(PROVIDER_ID, id.0.as_str(), CRED_FIELD, &merged)?;
                    tracing::warn!(
                        account = %id,
                        "claude live capture missing refresh token; kept existing refresh token in store, only updated access token"
                    );
                    return Ok(());
                }
            }
        }
    }

    let serialized = serde_json::to_string(&live_creds)?;
    store.set(PROVIDER_ID, id.0.as_str(), CRED_FIELD, &serialized)?;
    tracing::debug!(account = %id, "claude live credentials captured into store before swap");
    Ok(())
}

/// best-effort 预刷新。返回 `true` 表示 token 已被刷新。
async fn best_effort_pre_refresh(creds: &mut CredentialsFile) -> bool {
    if !is_expired_or_soon(creds, settings::current().token.refresh_slack_ms) {
        return false;
    }
    if creds
        .oauth
        .refresh_token
        .as_deref()
        .unwrap_or("")
        .is_empty()
    {
        tracing::warn!(
            "token expired/expiring but refreshToken is empty in store; skipping pre-refresh — log in again if the client returns 401"
        );
        return false;
    }
    match apply_refresh_to_creds(creds).await {
        Ok(()) => {
            tracing::info!("Claude access_token pre-refreshed");
            true
        }
        Err(e) => {
            tracing::warn!(
                err=%e,
                "pre-refresh failed; swapping with existing token — log in again if the client returns 401"
            );
            false
        }
    }
}

/// 执行一次 OAuth refresh 并把响应应用到 `creds`。
///
/// 不读 keyring、不写 keyring、不动磁盘；调用方负责持久化。缺 `refresh_token`
/// 时返回 [`Error::Provider`]（不能 offline 续期）。
async fn apply_refresh_to_creds(creds: &mut CredentialsFile) -> Result<()> {
    let refresh_token = creds
        .oauth
        .refresh_token
        .clone()
        .filter(|rt| !rt.is_empty())
        .ok_or_else(|| {
            Error::Provider(format!(
                "{PROVIDER_ID} account has no refreshToken; cannot refresh offline, log in and re-add"
            ))
        })?;
    let resp = oauth::refresh_access_token(&refresh_token, &creds.oauth.scopes).await?;
    creds.oauth.access_token = resp.access_token;
    if let Some(secs) = resp.expires_in {
        creds.oauth.expires_at = Some(Utc::now().timestamp_millis() + secs * 1000);
    }
    if let Some(rt) = resp.refresh_token {
        creds.oauth.refresh_token = Some(rt);
    }
    Ok(())
}

/// 判断 quota 拉取错误是否为鉴权失效(401)，用于决定是否触发刷新重试。
fn is_auth_error(err: &Error) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("401") || s.contains("unauthorized")
}

/// refresh endpoint 是否回 `invalid_grant`(refresh token 被服务端作废)。
/// 与网络/超时等瞬时错误区分：只有它才把 token 标记为「死」并停止重试。
fn is_invalid_grant(err: &Error) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("invalid_grant")
}

/// refresh token 的短指纹(sha256 前 8 字节 hex)。只用于内存去重，不落盘、不外泄原 token。
fn refresh_fingerprint(refresh_token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(refresh_token.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// parked 账号 refresh token 已死时返回的统一错误。文本含 "re-login"，供 CLI 压成
/// `needs re-login` 展示，而不是默默挂旧缓存。
fn relogin_required_error(id: &AccountId) -> Error {
    Error::QuotaFetch(format!(
        "re-login required for {PROVIDER_ID}:{id}; refresh token invalid, log in again in Claude Code"
    ))
}

fn is_expired_or_soon(creds: &CredentialsFile, slack_ms: i64) -> bool {
    let Some(expires_at_ms) = creds.oauth.expires_at else {
        return false;
    };
    let now_ms = Utc::now().timestamp_millis();
    expires_at_ms <= now_ms + slack_ms
}

fn make_quota(
    id: &AccountId,
    window: QuotaWindow,
    util_value: Option<f64>,
    reset_at: Option<chrono::DateTime<Utc>>,
) -> Quota {
    // Anthropic usage 的 utilization 固定是 0~100 的已用百分比。
    // 不能把小于 1 的值当成比例，否则 0.97% 会被误判成 97%。
    let (used, status) = match util_value {
        Some(v) if v.is_finite() => {
            let pct = v;
            let used = pct.round().clamp(0.0, 100.0) as u64;
            (used, QuotaStatus::from_percent(pct))
        }
        _ => (0, QuotaStatus::Unknown),
    };
    Quota {
        provider: PROVIDER_ID.into(),
        account_id: id.clone(),
        window,
        used,
        limit: if util_value.is_some() { 100 } else { 0 },
        reset_at,
        status,
        note: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁死隔离钥匙串 service 名公式：`Claude Code-credentials-<sha256(dir).hex[:8]>`。
    /// 已知向量：sha256("abc") = ba7816bf...，前 8 位 = "ba7816bf"。
    #[cfg(target_os = "macos")]
    #[test]
    fn isolated_keychain_service_matches_formula() {
        assert_eq!(
            isolated_keychain_service("abc"),
            "Claude Code-credentials-ba7816bf"
        );
    }

    fn mk_creds(expires_at: Option<i64>, refresh: Option<&str>) -> CredentialsFile {
        CredentialsFile {
            oauth: crate::claude_files::ClaudeOauth {
                access_token: "old-token".into(),
                refresh_token: refresh.map(str::to_string),
                expires_at,
                scopes: vec![],
                other: serde_json::Map::new(),
            },
            other: serde_json::Map::new(),
        }
    }

    #[test]
    fn is_expired_or_soon_handles_none() {
        let c = mk_creds(None, None);
        assert!(!is_expired_or_soon(&c, 60_000));
    }

    #[test]
    fn is_expired_or_soon_true_when_past() {
        let past = Utc::now().timestamp_millis() - 60_000;
        let c = mk_creds(Some(past), None);
        assert!(is_expired_or_soon(&c, 60_000));
    }

    #[test]
    fn is_expired_or_soon_true_when_within_slack() {
        let near = Utc::now().timestamp_millis() + 60_000;
        let c = mk_creds(Some(near), None);
        assert!(is_expired_or_soon(&c, 5 * 60_000));
    }

    #[test]
    fn is_expired_or_soon_false_when_safely_future() {
        let future = Utc::now().timestamp_millis() + 24 * 60 * 60_000;
        let c = mk_creds(Some(future), None);
        assert!(!is_expired_or_soon(&c, 60_000));
    }

    #[test]
    fn make_quota_percent_input() {
        let q = make_quota(
            &AccountId("x".into()),
            QuotaWindow::FiveHour,
            Some(42.0),
            None,
        );
        assert_eq!(q.used, 42);
        assert_eq!(q.limit, 100);
        assert_eq!(q.status, QuotaStatus::Ok);
    }

    #[test]
    fn make_quota_small_percent_is_not_treated_as_ratio() {
        let q = make_quota(
            &AccountId("x".into()),
            QuotaWindow::FiveHour,
            Some(0.97),
            None,
        );
        assert_eq!(q.used, 1);
        assert_eq!(q.status, QuotaStatus::Ok);
    }

    #[test]
    fn make_quota_exhausted() {
        let q = make_quota(
            &AccountId("x".into()),
            QuotaWindow::FiveHour,
            Some(100.0),
            None,
        );
        assert_eq!(q.status, QuotaStatus::Exhausted);
    }

    #[test]
    fn dead_refresh_guard_tracks_by_token_and_recovers_on_rotation() {
        use subswap_core::FileStore;
        let tmp = tempfile::tempdir().unwrap();
        let provider = ClaudeProvider {
            store: Arc::new(FileStore::new(tmp.path().join("creds.json"))),
            registry: Arc::new(AccountRegistry::new(tmp.path().join("registry.toml"))),
            claude_home: tmp.path().join("claude"),
            dead_refresh: Arc::new(Mutex::new(HashSet::new())),
        };
        // 空 token 永不判死;未标记前不算死。
        assert!(!provider.is_refresh_dead(""));
        assert!(!provider.is_refresh_dead("rt-old"));
        provider.mark_refresh_dead("rt-old");
        assert!(provider.is_refresh_dead("rt-old"));
        // 轮换出的新 token 指纹不同 → 自动恢复刷新。
        assert!(!provider.is_refresh_dead("rt-new"));
    }

    #[test]
    fn invalid_grant_classification_and_relogin_error_text() {
        assert!(is_invalid_grant(&Error::QuotaFetch(
            "refresh returned 400: {\"error\":\"invalid_grant\"}".into()
        )));
        assert!(!is_invalid_grant(&Error::QuotaFetch(
            "request timeout".into()
        )));
        // 透出给 CLI 的错误文本含 "re-login",供 compact_error 归一成 needs re-login。
        let msg = relogin_required_error(&AccountId("a@x.com".into())).to_string();
        assert!(msg.contains("re-login"), "{msg}");
    }

    #[test]
    fn isolated_home_shares_everything_except_account_files() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let isolated = tmp.path().join("isolated");
        std::fs::create_dir_all(global.join("projects")).unwrap();
        std::fs::create_dir_all(global.join("plugins")).unwrap();
        std::fs::write(global.join("projects/session.jsonl"), "session").unwrap();
        std::fs::write(global.join("plugins/plugin.json"), "{}").unwrap();
        std::fs::write(global.join("CLAUDE.md"), "@AGENTS.md").unwrap();
        std::fs::write(global.join(".credentials.json"), "global-credentials").unwrap();
        std::fs::write(
            global.join("settings.json"),
            r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"api-token","KEEP":"yes"},"permissions":{"allow":["Read"]}}"#,
        )
        .unwrap();

        link_shared_claude_home(&global, &isolated).unwrap();

        assert!(std::fs::symlink_metadata(isolated.join("projects"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(isolated.join("plugins"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(isolated.join("CLAUDE.md"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!isolated.join(".credentials.json").exists());

        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(isolated.join("settings.json")).unwrap())
                .unwrap();
        assert!(settings["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        assert_eq!(settings["env"]["KEEP"], "yes");
        assert_eq!(settings["permissions"]["allow"][0], "Read");
    }

    #[test]
    fn isolated_home_merges_old_projects_before_linking() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("global");
        let isolated = tmp.path().join("isolated");
        std::fs::create_dir_all(global.join("projects")).unwrap();
        std::fs::create_dir_all(isolated.join("projects")).unwrap();
        std::fs::write(isolated.join("projects/old.jsonl"), "old").unwrap();

        link_shared_claude_home(&global, &isolated).unwrap();

        assert_eq!(
            std::fs::read_to_string(global.join("projects/old.jsonl")).unwrap(),
            "old"
        );
        assert!(std::fs::symlink_metadata(isolated.join("projects"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// 物化隔离目录后 `.claude.json` 必须含 `hasCompletedOnboarding = true`。
    /// 否则 claude 会对每个隔离会话跑「Select login method」首次引导流程，
    /// 即使钥匙串里已有有效凭证也一样（根因见 ACCOUNT_ISOLATION_DESIGN.md §2.3）。
    #[test]
    fn materialize_isolated_sets_has_completed_onboarding() {
        use crate::claude_files::mark_onboarding_complete;

        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("isolated");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = global_config_path(&config_dir);

        // 模拟 write_oauth_account_into_global 的初始状态：只有 oauthAccount。
        std::fs::write(
            &config_path,
            r#"{"oauthAccount":{"emailAddress":"test@x.com"}}"#,
        )
        .unwrap();

        mark_onboarding_complete(&config_path).unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            val["hasCompletedOnboarding"],
            serde_json::Value::Bool(true),
            "隔离目录 .claude.json 缺 hasCompletedOnboarding，claude 会弹首次引导登录框"
        );
        // 确保原有字段没被覆掉。
        assert_eq!(val["oauthAccount"]["emailAddress"], "test@x.com");
    }

    #[test]
    fn capture_on_leave_updates_store_for_owner() {
        use subswap_core::FileStore;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("claude");
        std::fs::create_dir_all(&home).unwrap();

        // live credentials.json:refreshToken=R2(Claude Code 刚轮换到的最新值)。
        let live_creds =
            r#"{"claudeAiOauth":{"accessToken":"AT2","refreshToken":"R2","expiresAt":111}}"#;
        std::fs::write(credentials_path(&home), live_creds).unwrap();
        std::fs::write(
            global_config_path(&home),
            r#"{"oauthAccount":{"emailAddress":"a@x.com"}}"#,
        )
        .unwrap();

        let store = FileStore::new(tmp.path().join("creds.json"));
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));

        // store 先放陈旧副本 R1。
        store
            .set(
                PROVIDER_ID,
                "a@x.com",
                CRED_FIELD,
                r#"{"claudeAiOauth":{"accessToken":"AT1","refreshToken":"R1"}}"#,
            )
            .unwrap();
        registry
            .upsert(Account {
                provider: PROVIDER_ID.into(),
                id: AccountId("a@x.com".into()),
                label: "a@x.com".into(),
                active: true,
                created_at: Utc::now(),
                last_used_at: None,
                priority: 100,
                extra: serde_json::Map::new(),
            })
            .unwrap();

        capture_live_into_store(&store, &registry, &home, false).unwrap();

        // 回灌后 store 应反映 live 的 R2。
        let stored = store
            .get(PROVIDER_ID, "a@x.com", CRED_FIELD)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "R2");
    }

    #[test]
    fn capture_on_leave_preserves_refresh_when_live_missing_it() {
        use subswap_core::FileStore;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("claude");
        std::fs::create_dir_all(&home).unwrap();

        // live credentials.json:Claude Code 轮换期间钥匙串短暂缺 refreshToken。
        let live_creds = r#"{"claudeAiOauth":{"accessToken":"AT2","expiresAt":222}}"#;
        std::fs::write(credentials_path(&home), live_creds).unwrap();
        std::fs::write(
            global_config_path(&home),
            r#"{"oauthAccount":{"emailAddress":"a@x.com"}}"#,
        )
        .unwrap();

        let store = FileStore::new(tmp.path().join("creds.json"));
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));

        // store 里已有可续期的 refresh token R1，不能被这次回灌抹空。
        store
            .set(
                PROVIDER_ID,
                "a@x.com",
                CRED_FIELD,
                r#"{"claudeAiOauth":{"accessToken":"AT1","refreshToken":"R1"}}"#,
            )
            .unwrap();
        registry
            .upsert(Account {
                provider: PROVIDER_ID.into(),
                id: AccountId("a@x.com".into()),
                label: "a@x.com".into(),
                active: true,
                created_at: Utc::now(),
                last_used_at: None,
                priority: 100,
                extra: serde_json::Map::new(),
            })
            .unwrap();

        capture_live_into_store(&store, &registry, &home, false).unwrap();

        // refresh token 必须保留(R1),access token 跟进最新值(AT2)。
        let stored = store
            .get(PROVIDER_ID, "a@x.com", CRED_FIELD)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "R1");
        assert_eq!(v["claudeAiOauth"]["accessToken"], "AT2");
    }

    #[test]
    fn capture_on_leave_overwrites_when_neither_side_has_refresh() {
        use subswap_core::FileStore;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("claude");
        std::fs::create_dir_all(&home).unwrap();

        let live_creds = r#"{"claudeAiOauth":{"accessToken":"AT2"}}"#;
        std::fs::write(credentials_path(&home), live_creds).unwrap();
        std::fs::write(
            global_config_path(&home),
            r#"{"oauthAccount":{"emailAddress":"a@x.com"}}"#,
        )
        .unwrap();

        let store = FileStore::new(tmp.path().join("creds.json"));
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));

        // store 里原本也没有 refresh token：两边都缺时维持原行为，正常覆盖。
        store
            .set(
                PROVIDER_ID,
                "a@x.com",
                CRED_FIELD,
                r#"{"claudeAiOauth":{"accessToken":"AT1"}}"#,
            )
            .unwrap();
        registry
            .upsert(Account {
                provider: PROVIDER_ID.into(),
                id: AccountId("a@x.com".into()),
                label: "a@x.com".into(),
                active: true,
                created_at: Utc::now(),
                last_used_at: None,
                priority: 100,
                extra: serde_json::Map::new(),
            })
            .unwrap();

        capture_live_into_store(&store, &registry, &home, false).unwrap();

        let stored = store
            .get(PROVIDER_ID, "a@x.com", CRED_FIELD)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(v["claudeAiOauth"]["accessToken"], "AT2");
        assert!(v["claudeAiOauth"]["refreshToken"].is_null());
    }

    #[test]
    fn capture_on_leave_skips_unmanaged_account() {
        use subswap_core::FileStore;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("claude");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            credentials_path(&home),
            r#"{"claudeAiOauth":{"accessToken":"AT","refreshToken":"R"}}"#,
        )
        .unwrap();
        std::fs::write(
            global_config_path(&home),
            r#"{"oauthAccount":{"emailAddress":"unmanaged@x.com"}}"#,
        )
        .unwrap();

        let store = FileStore::new(tmp.path().join("creds.json"));
        let registry = AccountRegistry::new(tmp.path().join("registry.toml"));

        // 该账号未注册 → 不回灌。
        capture_live_into_store(&store, &registry, &home, false).unwrap();
        assert!(store
            .get(PROVIDER_ID, "unmanaged@x.com", CRED_FIELD)
            .unwrap()
            .is_none());
    }

    #[test]
    fn active_account_id_reads_oauth_account() {
        use subswap_core::FileStore;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("claude");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            global_config_path(&home),
            r#"{"oauthAccount":{"emailAddress":"active@x.com"}}"#,
        )
        .unwrap();

        let provider = ClaudeProvider {
            store: Arc::new(FileStore::new(tmp.path().join("creds.json"))),
            registry: Arc::new(AccountRegistry::new(tmp.path().join("registry.toml"))),
            claude_home: home,
            dead_refresh: Arc::new(Mutex::new(HashSet::new())),
        };
        assert_eq!(
            provider.active_account_id().unwrap(),
            Some(AccountId("active@x.com".into()))
        );
    }

    // ── reconcile_api_external_login 测试 ──

    /// 准备 reconcile 测试的公共 fixture。
    struct ReconFixture {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        provider: ClaudeProvider,
    }

    impl ReconFixture {
        fn new() -> Self {
            use subswap_core::FileStore;
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("claude");
            std::fs::create_dir_all(&home).unwrap();
            let provider = ClaudeProvider {
                store: Arc::new(FileStore::new(tmp.path().join("creds.json"))),
                registry: Arc::new(AccountRegistry::new(tmp.path().join("registry.toml"))),
                claude_home: home.clone(),
                dead_refresh: Arc::new(Mutex::new(HashSet::new())),
            };
            Self {
                _tmp: tmp,
                home,
                provider,
            }
        }

        fn write_settings(&self, json: &str) {
            std::fs::write(settings_path(&self.home), json).unwrap();
        }

        fn write_global(&self, json: &str) {
            std::fs::write(global_config_path(&self.home), json).unwrap();
        }

        fn write_api_state(
            &self,
            account_id: &str,
            restore_env: serde_json::Map<String, serde_json::Value>,
        ) {
            self.write_api_state_with_email(account_id, None, restore_env);
        }

        fn write_api_state_with_email(
            &self,
            account_id: &str,
            oauth_email: Option<&str>,
            restore_env: serde_json::Map<String, serde_json::Value>,
        ) {
            write_api_state(
                &api_state_path(&self.home),
                &ApiState {
                    account_id: account_id.into(),
                    oauth_email: oauth_email.map(str::to_string),
                    restore_env,
                },
            )
            .unwrap();
        }

        fn api_state_exists(&self) -> bool {
            api_state_path(&self.home).exists()
        }

        fn settings_contains_key(&self, key: &str) -> bool {
            let v = read_settings(&settings_path(&self.home)).unwrap();
            v.get("env").and_then(|e| e.get(key)).is_some()
        }
    }

    #[test]
    fn reconcile_skips_when_not_in_api_mode() {
        let f = ReconFixture::new();
        f.write_settings(r#"{"env":{"ANTHROPIC_MODEL":"old"}}"#);
        f.write_global(r#"{"oauthAccount":{"emailAddress":"a@x.com"}}"#);

        f.provider.reconcile_api_external_login().unwrap();

        // 没有哨兵文件 → 什么都不动。
        assert!(!f.api_state_exists());
        assert!(f.settings_contains_key("ANTHROPIC_MODEL"));
    }

    #[test]
    fn reconcile_skips_when_api_matches_oauth() {
        let f = ReconFixture::new();
        f.write_settings(r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.deepseek.com","ANTHROPIC_MODEL":"deepseek"}}"#);
        f.write_global(r#"{"oauthAccount":{"emailAddress":"alice@x.com"}}"#);
        // oauth_email 与 live oauthAccount 一致 → 不动。
        f.write_api_state_with_email("deepseek", Some("alice@x.com"), serde_json::Map::new());

        f.provider.reconcile_api_external_login().unwrap();

        assert!(f.api_state_exists());
        assert!(f.settings_contains_key("ANTHROPIC_BASE_URL"));
    }

    /// 旧版哨兵文件没有 oauth_email 字段时，无法判断是否有外部登录，保守跳过。
    /// 典型场景：用户有 DeepSeek 等自定义 API 账号（account_id = "deepseek"），
    /// 同时 Claude Code OAuth 账号在 global.json 里，两者邮箱永远不等，不能用 account_id 比较。
    #[test]
    fn reconcile_skips_when_no_oauth_email_in_state() {
        let f = ReconFixture::new();
        f.write_settings(r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.deepseek.com"}}"#);
        f.write_global(r#"{"oauthAccount":{"emailAddress":"scottethanjfgg@gmail.com"}}"#);
        // 旧格式：account_id 是自定义 ID，没有 oauth_email。
        f.write_api_state("deepseek", serde_json::Map::new());

        f.provider.reconcile_api_external_login().unwrap();

        // 无 oauth_email → 跳过 reconcile，API 模式不动。
        assert!(f.api_state_exists());
        assert!(f.settings_contains_key("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn reconcile_skips_when_oauth_absent() {
        let f = ReconFixture::new();
        f.write_settings(r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.deepseek.com"}}"#);
        f.write_global(r#"{"projects":[]}"#); // 无 oauthAccount
        f.write_api_state("deepseek@x.com", serde_json::Map::new());

        f.provider.reconcile_api_external_login().unwrap();

        // 无 live oauthAccount → 保持 API 模式。
        assert!(f.api_state_exists());
        assert!(f.settings_contains_key("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn reconcile_restores_and_deletes_on_mismatch() {
        let f = ReconFixture::new();
        let restore =
            serde_json::from_value(serde_json::json!({"ANTHROPIC_MODEL":"old-model"})).unwrap();
        f.write_settings(
            r#"{"env":{"ANTHROPIC_MODEL":"deepseek","ANTHROPIC_BASE_URL":"https://api.deepseek.com","KEEP":"yes"}}"#,
        );
        // live oauthAccount 变成了 new-user，与切入 API 时记录的 alice 不符 → 触发 reconcile。
        f.write_global(r#"{"oauthAccount":{"emailAddress":"new-user@x.com"}}"#);
        f.write_api_state_with_email("deepseek", Some("alice@x.com"), restore);

        f.provider.reconcile_api_external_login().unwrap();

        // 哨兵已删除。
        assert!(!f.api_state_exists());
        // 受管字段恢复为切入 API 前的值。
        let settings = read_settings(&settings_path(&f.home)).unwrap();
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "old-model");
        // 非受管字段保留。
        assert_eq!(settings["env"]["KEEP"], "yes");
        // API 专用字段已移除。
        assert!(settings["env"].get("ANTHROPIC_BASE_URL").is_none());
    }

    #[test]
    fn reconcile_idempotent_on_retry() {
        let f = ReconFixture::new();
        f.write_settings(r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.deepseek.com"}}"#);
        f.write_global(r#"{"oauthAccount":{"emailAddress":"new-user@x.com"}}"#);
        f.write_api_state_with_email(
            "deepseek",
            Some("alice@x.com"),
            serde_json::from_value(serde_json::json!({})).unwrap(),
        );

        // 第一次 — 执行调和。
        f.provider.reconcile_api_external_login().unwrap();
        assert!(!f.api_state_exists());
        assert!(!f.settings_contains_key("ANTHROPIC_BASE_URL"));

        // 第二次 — 无哨兵文件，直接短路返回 Ok。
        f.provider.reconcile_api_external_login().unwrap();
        assert!(!f.api_state_exists());
    }
}

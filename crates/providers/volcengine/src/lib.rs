//! 火山方舟 Coding Plan 独立 Provider。
//!
//! Key 与账号归 `volcengine` 命名空间；激活目标只是 Claude Code 的
//! Anthropic 兼容环境变量，不复用 Claude 账号，也不参与自动换号。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use sha2::{Digest, Sha256};
use subswap_core::error::{Error, Result};
use subswap_core::swap::{swap_with_snapshot, SwapTarget};
use subswap_core::{
    Account, AccountId, AccountRegistry, ClientTarget, CredentialStore, OfficialDisconnect,
    Provider, Quota,
};
use subswap_provider_claude::{
    api_state_path, capture_managed_env, claude_home, global_config_path, read_api_state,
    read_oauth_account, read_settings, remove_api_state, restore_oauth_env_in_settings,
    settings_path, write_api_env_into_settings, write_api_state, ApiState, ClaudeApiConfig,
};
use subswap_provider_common::EnvIsolatedProvider;

mod quota;

/// 独立 Provider 标识。
pub const PROVIDER_ID: &str = "volcengine";
/// 官方 Coding Plan Anthropic 兼容端点。
pub const CODING_PLAN_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/coding";
/// 控制台托管模型路由的默认 Model Name。
pub const DEFAULT_MODEL: &str = "ark-code-latest";
const API_KEY_FIELD: &str = "api_key";
/// 额度查询用的火山 AK（与推理 Key 分开存放）。
pub(crate) const QUOTA_AK_FIELD: &str = "access_key_id";
/// 额度查询用的火山 SK（与推理 Key 分开存放）。
pub(crate) const QUOTA_SK_FIELD: &str = "secret_access_key";

/// 火山方舟 Coding Plan Provider。
#[derive(Clone)]
pub struct VolcengineProvider {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    claude_home: PathBuf,
}

/// 构造使用标准 Claude 配置目录的 Provider。
pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> VolcengineProvider {
    VolcengineProvider {
        store,
        registry,
        claude_home: claude_home(),
    }
}

impl VolcengineProvider {
    /// 构造使用指定 Claude 配置目录的 Provider，供隔离测试与便携运行使用。
    pub fn with_home(
        store: Arc<dyn CredentialStore>,
        registry: Arc<AccountRegistry>,
        claude_home: PathBuf,
    ) -> Self {
        Self {
            store,
            registry,
            claude_home,
        }
    }

    /// 导入一把 Coding Plan Key；账号 id 只含指纹，label 只含末四位。
    pub fn import_key(&self, api_key: &str, active: Option<bool>) -> Result<Account> {
        self.upsert_key(api_key, default_config(), active)
    }

    /// 从当前 Claude settings 导入官方 Coding Plan 端点已有的 Key 与模型配置。
    pub fn import_active(&self) -> Result<Account> {
        let (api_key, config) = self.current_live_config()?.ok_or_else(|| {
            Error::Provider(
                "no Volcengine Coding Plan configuration in Claude settings; pass an API key after `--`"
                    .into(),
            )
        })?;
        self.upsert_key(&api_key, config, Some(true))
    }

    /// 返回当前属于 Volcengine 的 live 账号；其他 Provider 的 API 模式不算。
    pub fn live_account_id(&self) -> Result<AccountId> {
        if let Some(state) = read_api_state(&api_state_path(&self.claude_home))? {
            if state.belongs_to(PROVIDER_ID) {
                return Ok(AccountId(state.account_id));
            }
            return Err(Error::Provider(
                "Claude API mode belongs to another provider".into(),
            ));
        }
        let (api_key, _) = self
            .current_live_config()?
            .ok_or_else(|| Error::Provider("no active Volcengine Coding Plan account".into()))?;
        Ok(account_id_for_key(&api_key))
    }

    /// 同步当前账号元数据；优先认独立哨兵，否则导入官方端点的现有配置。
    pub fn sync_active_metadata(&self) -> Result<Account> {
        if let Some(state) = read_api_state(&api_state_path(&self.claude_home))? {
            if state.belongs_to(PROVIDER_ID) {
                let id = AccountId(state.account_id);
                return self.registry.find(PROVIDER_ID, &id)?.ok_or_else(|| {
                    Error::AccountNotFound {
                        provider: PROVIDER_ID.into(),
                        id: id.to_string(),
                    }
                });
            }
        }
        self.import_active()
    }

    /// 删除当前账号时恢复进入 API 模式前的 Claude 配置；删 parked 账号不写 live。
    pub async fn disconnect_official(&self, id: &AccountId) -> Result<OfficialDisconnect> {
        let id = id.clone();
        let home = self.claude_home.clone();
        tokio::task::spawn_blocking(move || {
            let state_path = api_state_path(&home);
            let matches = read_api_state(&state_path)?
                .is_some_and(|state| state.belongs_to(PROVIDER_ID) && state.account_id == id.0);
            if !matches {
                return Ok(OfficialDisconnect::AlreadyGone);
            }
            let state = read_api_state(&state_path)?.ok_or_else(|| {
                Error::Provider("Volcengine API marker disappeared during disconnect".into())
            })?;
            let settings = settings_path(&home);
            swap_with_snapshot(
                PROVIDER_ID,
                &home,
                vec![
                    SwapTarget {
                        snapshot_name: "settings.json",
                        live_path: settings,
                        writer: Box::new(move |path| {
                            restore_oauth_env_in_settings(path, &state.restore_env)
                        }),
                    },
                    SwapTarget {
                        snapshot_name: "api-state.json",
                        live_path: state_path,
                        writer: Box::new(remove_api_state),
                    },
                ],
                || Ok(()),
            )?;
            Ok(OfficialDisconnect::AlreadyGone)
        })
        .await
        .map_err(|e| Error::Provider(format!("Volcengine disconnect join failed: {e}")))?
    }

    fn current_live_config(&self) -> Result<Option<(String, ClaudeApiConfig)>> {
        let settings = read_settings(&settings_path(&self.claude_home))?;
        let Some(env) = settings.get("env").and_then(serde_json::Value::as_object) else {
            return Ok(None);
        };
        let Some(base_url) = env.get("ANTHROPIC_BASE_URL").and_then(as_nonempty_str) else {
            return Ok(None);
        };
        if base_url.trim_end_matches('/') != CODING_PLAN_BASE_URL {
            return Ok(None);
        }
        let Some(api_key) = env.get("ANTHROPIC_AUTH_TOKEN").and_then(as_nonempty_str) else {
            return Ok(None);
        };
        let model = |key: &str| {
            env.get(key)
                .and_then(as_nonempty_str)
                .unwrap_or(DEFAULT_MODEL)
                .to_string()
        };
        let sonnet = model("ANTHROPIC_DEFAULT_SONNET_MODEL");
        let haiku = model("ANTHROPIC_DEFAULT_HAIKU_MODEL");
        let default_model = env
            .get("ANTHROPIC_MODEL")
            .and_then(as_nonempty_str)
            .unwrap_or(&sonnet)
            .to_string();
        let config = ClaudeApiConfig {
            base_url: CODING_PLAN_BASE_URL.into(),
            auth_field: "ANTHROPIC_AUTH_TOKEN".into(),
            model: default_model,
            opus_model: model("ANTHROPIC_DEFAULT_OPUS_MODEL"),
            sonnet_model: sonnet,
            haiku_model: haiku.clone(),
            subagent_model: env
                .get("CLAUDE_CODE_SUBAGENT_MODEL")
                .and_then(as_nonempty_str)
                .unwrap_or(&haiku)
                .to_string(),
            effort_level: env
                .get("CLAUDE_CODE_EFFORT_LEVEL")
                .and_then(as_nonempty_str)
                .unwrap_or_default()
                .to_string(),
        };
        Ok(Some((api_key.to_string(), config)))
    }

    fn upsert_key(
        &self,
        api_key: &str,
        config: ClaudeApiConfig,
        active: Option<bool>,
    ) -> Result<Account> {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(Error::Config(
                "Volcengine Coding Plan API key cannot be empty".into(),
            ));
        }
        // 提前验证认证字段，避免入库后才在首次切换失败。
        config.env(api_key)?;
        let id = account_id_for_key(api_key);
        let existing = self.registry.find(PROVIDER_ID, &id)?;
        self.store.set(PROVIDER_ID, &id.0, API_KEY_FIELD, api_key)?;
        let mut extra = serde_json::Map::new();
        extra.insert("kind".into(), "api".into());
        extra.insert("manual_only".into(), true.into());
        extra.insert("billing".into(), "flat".into());
        extra.insert("api_config".into(), serde_json::to_value(config)?);
        let account = Account {
            provider: PROVIDER_ID.into(),
            id: id.clone(),
            label: label_for_key(api_key),
            active: active.unwrap_or_else(|| existing.as_ref().is_some_and(|a| a.active)),
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

    /// 设置额度查询用的火山 AK/SK（与推理 Key 分开存放）。
    pub fn set_quota_credentials(
        &self,
        id: &AccountId,
        access_key_id: &str,
        secret_access_key: &str,
    ) -> Result<()> {
        let access_key_id = access_key_id.trim();
        let secret_access_key = secret_access_key.trim();
        if access_key_id.is_empty() || secret_access_key.is_empty() {
            return Err(Error::Config(
                "Volcengine AccessKey ID and Secret cannot be empty".into(),
            ));
        }
        self.registry
            .find(PROVIDER_ID, id)?
            .ok_or_else(|| Error::AccountNotFound {
                provider: PROVIDER_ID.into(),
                id: id.to_string(),
            })?;
        self.store
            .set(PROVIDER_ID, &id.0, QUOTA_AK_FIELD, access_key_id)?;
        self.store.set(
            PROVIDER_ID,
            &id.0,
            QUOTA_SK_FIELD,
            secret_access_key,
        )?;
        Ok(())
    }

    /// 读取额度查询用的 AK/SK；缺失时返回 `None`，调用方再试 arkcli 兜底。
    fn quota_credentials(&self, id: &AccountId) -> Result<(Option<String>, Option<String>)> {
        let ak = self.store.get(PROVIDER_ID, &id.0, QUOTA_AK_FIELD)?;
        let sk = self.store.get(PROVIDER_ID, &id.0, QUOTA_SK_FIELD)?;
        let ak = ak.map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
        let sk = sk.map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
        Ok((ak, sk))
    }

    fn account_and_env(
        &self,
        id: &AccountId,
    ) -> Result<(Account, serde_json::Map<String, serde_json::Value>)> {
        let account =
            self.registry
                .find(PROVIDER_ID, id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                })?;
        let api_key = self
            .store
            .get(PROVIDER_ID, &id.0, API_KEY_FIELD)?
            .ok_or_else(|| {
                Error::Credential(format!(
                    "no API key for {PROVIDER_ID}:{id}; run `subswap login volcengine -- <key>`"
                ))
            })?;
        let config: ClaudeApiConfig =
            serde_json::from_value(account.extra.get("api_config").cloned().ok_or_else(|| {
                Error::Provider(format!(
                    "registry entry {PROVIDER_ID}:{id} missing api_config field"
                ))
            })?)?;
        Ok((account, config.env(&api_key)?))
    }
}

fn default_config() -> ClaudeApiConfig {
    ClaudeApiConfig {
        base_url: CODING_PLAN_BASE_URL.into(),
        auth_field: "ANTHROPIC_AUTH_TOKEN".into(),
        model: DEFAULT_MODEL.into(),
        opus_model: DEFAULT_MODEL.into(),
        sonnet_model: DEFAULT_MODEL.into(),
        haiku_model: DEFAULT_MODEL.into(),
        subagent_model: DEFAULT_MODEL.into(),
        effort_level: String::new(),
    }
}

fn account_id_for_key(api_key: &str) -> AccountId {
    let digest = Sha256::digest(api_key.trim().as_bytes());
    let fingerprint = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    AccountId(format!("ark-{fingerprint}"))
}

fn label_for_key(api_key: &str) -> String {
    let suffix = api_key
        .trim()
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("Ark …{suffix}")
}

fn as_nonempty_str(value: &serde_json::Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[async_trait]
impl Provider for VolcengineProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "Volcengine Ark Coding Plan"
    }

    fn client_targets(&self) -> Vec<ClientTarget> {
        vec![ClientTarget {
            id: "volcengine_claude_settings".into(),
            display_name: "Volcengine Claude settings".into(),
            probe_path: settings_path(&self.claude_home),
        }]
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(PROVIDER_ID)
    }

    async fn activate(&self, id: &AccountId) -> Result<()> {
        let provider = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            let (_, api_env) = provider.account_and_env(&id)?;
            let settings = settings_path(&provider.claude_home);
            let state_path = api_state_path(&provider.claude_home);
            let existing_state = read_api_state(&state_path)?;
            let restore_env = match &existing_state {
                Some(state) => state.restore_env.clone(),
                None => capture_managed_env(&read_settings(&settings)?),
            };
            let oauth_email = existing_state
                .and_then(|state| state.oauth_email)
                .or_else(|| {
                    read_oauth_account(&global_config_path(&provider.claude_home))
                        .ok()
                        .flatten()
                        .map(|account| account.email_address)
                });
            let state = ApiState {
                provider: PROVIDER_ID.into(),
                account_id: id.0.clone(),
                oauth_email,
                restore_env,
            };
            let registry = provider.registry.clone();
            swap_with_snapshot(
                PROVIDER_ID,
                &provider.claude_home,
                vec![
                    SwapTarget {
                        snapshot_name: "settings.json",
                        live_path: settings,
                        writer: Box::new(move |path| write_api_env_into_settings(path, &api_env)),
                    },
                    SwapTarget {
                        snapshot_name: "api-state.json",
                        live_path: state_path,
                        writer: Box::new(move |path| write_api_state(path, &state)),
                    },
                ],
                || registry.set_active(PROVIDER_ID, &id),
            )
        })
        .await
        .map_err(|e| Error::Provider(format!("Volcengine activation join failed: {e}")))?
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        let provider = self.clone();
        let id_for_creds = id.clone();
        let (access_key_id, secret_access_key) = tokio::task::spawn_blocking(move || {
            provider.quota_credentials(&id_for_creds)
        })
        .await
        .map_err(|e| Error::Provider(format!("read Volcengine quota credentials failed: {e}")))??
        ;
        quota::fetch_quota(id.clone(), access_key_id, secret_access_key).await
    }

    fn credential_store_fields(&self) -> &'static [&'static str] {
        &[API_KEY_FIELD, QUOTA_AK_FIELD, QUOTA_SK_FIELD]
    }
}

#[async_trait]
impl EnvIsolatedProvider for VolcengineProvider {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn native_cli(&self) -> &'static str {
        "claude"
    }

    async fn isolation_env(&self, id: &AccountId) -> Result<Option<Vec<(String, String)>>> {
        let provider = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            let (_, env) = provider.account_and_env(&id)?;
            Ok(Some(
                env.into_iter()
                    .filter_map(|(key, value)| value.as_str().map(|value| (key, value.to_string())))
                    .collect(),
            ))
        })
        .await
        .map_err(|e| Error::Provider(format!("Volcengine env isolation join failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subswap_core::FileStore;

    struct Fixture {
        _tmp: tempfile::TempDir,
        provider: VolcengineProvider,
        home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().join("claude");
            std::fs::create_dir_all(&home).unwrap();
            let provider = VolcengineProvider::with_home(
                Arc::new(FileStore::new(tmp.path().join("credentials.json"))),
                Arc::new(AccountRegistry::new(tmp.path().join("registry.toml"))),
                home.clone(),
            );
            Self {
                _tmp: tmp,
                provider,
                home,
            }
        }

        fn settings(&self) -> serde_json::Value {
            read_settings(&settings_path(&self.home)).unwrap()
        }
    }

    #[tokio::test]
    async fn independent_keys_activate_and_restore_original_settings() {
        let fixture = Fixture::new();
        std::fs::write(
            settings_path(&fixture.home),
            r#"{"env":{"ANTHROPIC_MODEL":"old-model","KEEP":"yes"}}"#,
        )
        .unwrap();
        let first = fixture.provider.import_key("first-secret", None).unwrap();
        assert_eq!(first.provider, "volcengine");
        assert!(first.id.0.starts_with("ark-"));
        assert!(!first.label.contains("first-secret"));
        assert_eq!(first.extra["billing"], "flat");
        assert_eq!(first.extra["manual_only"], true);

        fixture.provider.activate(&first.id).await.unwrap();
        let active = fixture.settings();
        assert_eq!(active["env"]["ANTHROPIC_BASE_URL"], CODING_PLAN_BASE_URL);
        assert_eq!(active["env"]["ANTHROPIC_AUTH_TOKEN"], "first-secret");
        assert_eq!(active["env"]["ANTHROPIC_MODEL"], DEFAULT_MODEL);
        assert_eq!(active["env"]["KEEP"], "yes");
        let state = read_api_state(&api_state_path(&fixture.home))
            .unwrap()
            .unwrap();
        assert!(state.belongs_to("volcengine"));

        let second = fixture.provider.import_key("second-secret", None).unwrap();
        fixture.provider.activate(&second.id).await.unwrap();
        let state = read_api_state(&api_state_path(&fixture.home))
            .unwrap()
            .unwrap();
        assert_eq!(state.restore_env["ANTHROPIC_MODEL"], "old-model");
        fixture
            .provider
            .disconnect_official(&second.id)
            .await
            .unwrap();
        let restored = fixture.settings();
        assert_eq!(restored["env"]["ANTHROPIC_MODEL"], "old-model");
        assert_eq!(restored["env"]["KEEP"], "yes");
        assert!(restored["env"].get("ANTHROPIC_BASE_URL").is_none());
        assert!(!api_state_path(&fixture.home).exists());
    }

    #[tokio::test]
    async fn env_isolation_uses_claude_without_touching_global_settings() {
        let fixture = Fixture::new();
        fixture
            .provider
            .import_key("isolated-secret", None)
            .unwrap();
        let id = fixture.provider.live_account_id().unwrap_err();
        assert!(id.to_string().contains("no active Volcengine"));
        let account = fixture.provider.sync_active_metadata().unwrap_err();
        assert!(account.to_string().contains("no Volcengine"));
        let account = fixture
            .provider
            .import_key("isolated-secret", None)
            .unwrap();
        let vars = fixture
            .provider
            .isolation_env(&account.id)
            .await
            .unwrap()
            .unwrap();
        assert!(vars.contains(&("ANTHROPIC_BASE_URL".into(), CODING_PLAN_BASE_URL.into())));
        assert!(vars.contains(&("ANTHROPIC_AUTH_TOKEN".into(), "isolated-secret".into())));
        assert!(!settings_path(&fixture.home).exists());
    }

    #[test]
    fn legacy_api_marker_defaults_to_claude() {
        let state: ApiState =
            serde_json::from_str(r#"{"account_id":"custom","restore_env":{},"oauth_email":null}"#)
                .unwrap();
        assert!(state.belongs_to("claude"));
        assert!(!state.belongs_to("volcengine"));
    }
}

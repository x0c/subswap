//! Pi coding agent Runtime Provider：`~/.pi/agent/` 下多 slot API key 的唯一写主。
//!
//! - 每个账号占 Pi `auth.json` 里一个 slot（`auth_slot`，如 `deepseek`）；
//!   切换 = 原子改写该 slot（保留其他 slot）+ 可选更新 `settings.json` 默认选择。
//! - 账号全部 `manual_only`（不参与自动换号）；`query_quota` 恒为空（Pi 不暴露额度）；
//!   `request_credential` 投影长效 key（`expires_at_ms=None`），永不刷新。
//! - 顶层 `volcengine` provider 只读保留：历史账号由 [`migration`] 搬进本抽屉，
//!   新 key 走 `subswap add-api pi --preset volcengine` 或 `subswap login volcengine -- <key>`。
//! - `models.json` 与 `sessions/` 永不触碰。

pub mod auth;
pub mod migration;
pub mod paths;
pub mod settings;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use subswap_core::error::{Error, Result};
use subswap_core::swap::{swap_with_snapshot, SwapTarget};
use subswap_core::{
    Account, AccountId, AccountRegistry, BillingKind, ClientTarget, CredentialStore,
    OfficialDisconnect, Provider, Quota, RequestCredential,
};
use subswap_provider_common::EnvIsolatedProvider;

pub use migration::migrate_legacy_volcengine_to_pi;

/// 独立 Provider 标识。
pub const PROVIDER_ID: &str = "pi";
/// 凭证仓库字段：API key。
pub const API_KEY_FIELD: &str = "api_key";

/// `deepseek` slot 的进程 env 映射（Pi 原生同样认它）。
const DEEPSEEK_ENV_VAR: &str = "DEEPSEEK_API_KEY";
/// `kimi-coding` slot 的进程 env 映射。
const KIMI_ENV_VAR: &str = "KIMI_API_KEY";

/// Pi Provider：凭证与选择状态的双写主。
#[derive(Clone)]
pub struct PiProvider {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    agent_dir: PathBuf,
}

/// 构造使用标准 Pi agent 目录的 Provider。
pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> PiProvider {
    PiProvider {
        store,
        registry,
        agent_dir: paths::pi_agent_dir(),
    }
}

/// 新建账号用的完整参数（`add-api pi` / 迁移 / live 同步共用）。
pub struct NewPiAccount {
    pub slot: String,
    pub native_provider: String,
    pub base_url: String,
    pub model: String,
    pub channel: String,
    pub billing: BillingKind,
}

/// 预设默认值（CLI 交互默认与 `--preset` 解析共用）。
pub struct PiPreset {
    pub slot: &'static str,
    pub native_provider: &'static str,
    pub endpoint: &'static str,
    pub model: &'static str,
    pub billing: BillingKind,
    /// 有 env 映射的 slot 才支持 `run` / `shell` / `env` 进程隔离。
    pub env_var: Option<&'static str>,
}

/// `add-api pi` 支持的预设：deepseek / volcengine / custom。
/// custom 无固定端点与模型，调用方必须显式传入。
pub fn preset(name: &str) -> Option<PiPreset> {
    match name.trim().to_ascii_lowercase().as_str() {
        "deepseek" => Some(PiPreset {
            slot: "deepseek",
            native_provider: "deepseek",
            endpoint: "https://api.deepseek.com/anthropic",
            model: "deepseek-chat",
            billing: BillingKind::Metered,
            env_var: Some(DEEPSEEK_ENV_VAR),
        }),
        "volcengine" | "ark" => Some(PiPreset {
            slot: "volcengine",
            native_provider: "volcengine",
            endpoint: auth::PI_VOLCENGINE_BASE_URL,
            model: auth::PI_VOLCENGINE_MODEL,
            billing: BillingKind::Flat,
            env_var: None,
        }),
        "custom" => Some(PiPreset {
            slot: "",
            native_provider: "custom",
            endpoint: "",
            model: "",
            billing: BillingKind::Metered,
            env_var: None,
        }),
        _ => None,
    }
}

impl PiProvider {
    /// 构造使用指定 agent 目录的 Provider，供隔离测试与便携运行使用。
    pub fn with_agent_dir(
        store: Arc<dyn CredentialStore>,
        registry: Arc<AccountRegistry>,
        agent_dir: PathBuf,
    ) -> Self {
        Self {
            store,
            registry,
            agent_dir,
        }
    }

    /// 当前 agent 目录（测试断言与诊断用）。
    pub fn agent_dir(&self) -> &std::path::Path {
        &self.agent_dir
    }

    /// 导入一把 key 为 Pi 账号；registry 已有同 id 则保留其 active。
    /// 只写凭证仓库与 registry，不碰任何 live 文件、不改 active 选择。
    pub fn import_key(&self, params: NewPiAccount, api_key: &str, active: Option<bool>) -> Result<Account> {
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(Error::Config("Pi API key cannot be empty".into()));
        }
        let slot = params.slot.trim();
        if slot.is_empty() {
            return Err(Error::Config("Pi auth slot cannot be empty".into()));
        }
        let id = AccountId(auth::account_id_for(slot, api_key));
        let existing = self.registry.find(PROVIDER_ID, &id)?;
        self.store.set(PROVIDER_ID, &id.0, API_KEY_FIELD, api_key)?;
        let mut extra = serde_json::Map::new();
        extra.insert("kind".into(), "api".into());
        extra.insert("channel".into(), params.channel.trim().to_string().into());
        extra.insert(
            "pi_api".into(),
            serde_json::json!({
                "native_provider": params.native_provider.trim(),
                "base_url": params.base_url.trim_end_matches('/'),
                "model": params.model.trim(),
                "auth_slot": slot,
            }),
        );
        extra.insert("manual_only".into(), true.into());
        extra.insert("billing".into(), params.billing.to_string().into());
        let account = Account {
            provider: PROVIDER_ID.into(),
            id: id.clone(),
            label: auth::label_for(slot, api_key),
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

    fn require_account(&self, id: &AccountId) -> Result<Account> {
        self.registry
            .find(PROVIDER_ID, id)?
            .ok_or_else(|| Error::AccountNotFound {
                provider: PROVIDER_ID.into(),
                id: id.to_string(),
            })
    }

    fn account_key(&self, account: &Account) -> Result<(String, String, String)> {
        let api_key = self
            .store
            .get(PROVIDER_ID, &account.id.0, API_KEY_FIELD)?
            .ok_or_else(|| {
                Error::Credential(format!(
                    "no API key for {PROVIDER_ID}:{}; re-import it",
                    account.id
                ))
            })?;
        let pi_api = account.extra.get("pi_api").ok_or_else(|| {
            Error::Provider(format!(
                "registry entry {PROVIDER_ID}:{} missing pi_api field",
                account.id
            ))
        })?;
        let slot = pi_api
            .get("auth_slot")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Provider(format!(
                    "registry entry {PROVIDER_ID}:{} missing pi_api.auth_slot",
                    account.id
                ))
            })?;
        let model = pi_api
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Ok((api_key, slot.to_string(), model))
    }

    /// 读 live 默认选择对应的账号 id（`settings.json` 的 `defaultProvider` slot 有 key 时）。
    pub fn live_account_id(&self) -> Result<AccountId> {
        let (provider, _) = settings::read_default(&self.agent_dir)?;
        let slot = provider.ok_or_else(|| {
            Error::Provider("no Pi default provider; run `subswap swap pi/<id>` first".into())
        })?;
        let key = auth::read_slot(&self.agent_dir, &slot)?.ok_or_else(|| {
            Error::Provider(format!("Pi auth slot {slot} is empty; re-import it"))
        })?;
        Ok(AccountId(auth::account_id_for(&slot, &key)))
    }

    /// 同步 live 默认 slot 的账号元数据（默认入口用）；slot 无 key 时报错。
    pub fn sync_active_metadata(&self) -> Result<Account> {
        let (provider, _) = settings::read_default(&self.agent_dir)?;
        let slot = provider.ok_or_else(|| {
            Error::Provider("no Pi default provider; run `subswap swap pi/<id>` first".into())
        })?;
        let key = auth::read_slot(&self.agent_dir, &slot)?.ok_or_else(|| {
            Error::Provider(format!("Pi auth slot {slot} is empty; re-import it"))
        })?;
        self.upsert_live_slot(&slot, &slot, &key)
    }

    /// 同步 live 全部 slot 进账号池；active 对齐默认 slot（`login pi` 用）。
    pub fn sync_slots(&self) -> Result<Vec<Account>> {
        let (default_slot, _) = settings::read_default(&self.agent_dir)?;
        let mut accounts = Vec::new();
        let mut active = None;
        for (slot, key) in auth::list_slots(&self.agent_dir)? {
            let account = self.upsert_live_slot(&slot, &slot, &key)?;
            if default_slot.as_deref() == Some(slot.as_str()) {
                active = Some(account.clone());
            }
            accounts.push(account);
        }
        if let Some(account) = &active {
            self.registry.set_active(PROVIDER_ID, &account.id)?;
        }
        Ok(accounts)
    }

    /// 把 live slot 的 key 落成 registry 账号（已存在则保留其 `pi_api` 元数据）。
    fn upsert_live_slot(&self, slot: &str, native_provider: &str, key: &str) -> Result<Account> {
        let id = AccountId(auth::account_id_for(slot, key));
        if let Some(existing) = self.registry.find(PROVIDER_ID, &id)? {
            return Ok(existing);
        }
        let known = preset(slot).filter(|p| !p.slot.is_empty());
        let preset_channel = match &known {
            Some(p) if p.slot == "volcengine" => auth::volcengine_channel(),
            Some(p) => p.slot.to_string(),
            None => slot.to_string(),
        };
        let (base_url, model, billing) = match &known {
            Some(p) => (p.endpoint.to_string(), p.model.to_string(), p.billing),
            None => (String::new(), String::new(), BillingKind::Flat),
        };
        self.import_key(
            NewPiAccount {
                slot: slot.to_string(),
                native_provider: native_provider.to_string(),
                base_url,
                model,
                channel: preset_channel,
                billing,
            },
            key,
            Some(false),
        )
    }

    /// 切换到指定账号：原子改写其 slot +（默认）更新默认选择，回滚安全。
    /// `update_default=false`（`swap --no-default`）时只写 slot，不碰 `settings.json`。
    /// 删除账号时尽力清除 live 里正好是该 key 的 slot；parked 只清本地。
    /// 从不因 active / manual_only 拒绝（由调用方统一保证）。
    pub async fn disconnect_official(&self, id: &AccountId) -> Result<OfficialDisconnect> {
        let provider = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            let account = match provider.require_account(&id) {
                Ok(account) => account,
                Err(_) => return Ok(OfficialDisconnect::AlreadyGone),
            };
            let (api_key, slot, _) = provider.account_key(&account)?;
            if auth::remove_slot_if_matching(&provider.agent_dir, &slot, &api_key)? {
                Ok(OfficialDisconnect::Disconnected)
            } else {
                Ok(OfficialDisconnect::AlreadyGone)
            }
        })
        .await
        .map_err(|e| Error::Provider(format!("Pi disconnect join failed: {e}")))?
    }
}

#[async_trait]
impl Provider for PiProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "Pi"
    }

    fn client_targets(&self) -> Vec<ClientTarget> {
        vec![ClientTarget {
            id: "pi_agent_auth".into(),
            display_name: "Pi agent auth".into(),
            probe_path: paths::auth_json_path(&self.agent_dir),
        }]
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(PROVIDER_ID)
    }

    async fn activate(&self, id: &AccountId) -> Result<()> {
        let provider = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || provider.activate_blocking(&id, true))
            .await
            .map_err(|e| Error::Provider(format!("Pi activation join failed: {e}")))?
    }

    /// Pi 不暴露额度接口：恒返回空（调用方显示缺额度，不触发自动切换）。
    async fn query_quota(&self, _id: &AccountId) -> Result<Vec<Quota>> {
        Ok(Vec::new())
    }

    /// 投影长效 API key（`expires_at_ms=None`）；永不刷新，不返回 refresh token。
    async fn request_credential(&self, id: &AccountId) -> Result<Option<RequestCredential>> {
        let provider = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            let account = provider.require_account(&id)?;
            let (api_key, _, _) = provider.account_key(&account)?;
            Ok(Some(RequestCredential {
                secret: api_key,
                expires_at_ms: None,
            }))
        })
        .await
        .map_err(|e| Error::Provider(format!("Pi credential join failed: {e}")))?
    }

    /// 按名分发之外的显式选择更新开关：`swap --no-default` 经此跳过 `settings.json`。
    /// 默认实现（其他 Provider）忽略该开关，保持原 `activate` 语义。
    async fn activate_with_default(&self, id: &AccountId, update_default: bool) -> Result<()> {
        let provider = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || provider.activate_blocking(&id, update_default))
            .await
            .map_err(|e| Error::Provider(format!("Pi activation join failed: {e}")))?
    }

    fn credential_store_fields(&self) -> &'static [&'static str] {
        &[API_KEY_FIELD]
    }
}

impl PiProvider {
    /// 阻塞式切换本体（调用方已在 `spawn_blocking` 内或另行包一层）。
    fn activate_blocking(&self, id: &AccountId, update_default: bool) -> Result<()> {
        let account = self.require_account(id)?;
        let (api_key, slot, model) = self.account_key(&account)?;
        let agent_dir = self.agent_dir.clone();
        let mut targets = Vec::new();
        let slot_for_auth = slot.clone();
        let key_for_auth = api_key.clone();
        let dir_for_auth = agent_dir.clone();
        targets.push(SwapTarget {
            snapshot_name: "auth.json",
            live_path: paths::auth_json_path(&agent_dir),
            writer: Box::new(move |path| {
                let _ = path;
                auth::write_slot_merged(&dir_for_auth, &slot_for_auth, &key_for_auth)
            }),
        });
        if update_default {
            let dir_for_settings = self.agent_dir.clone();
            let slot_for_settings = slot.clone();
            targets.push(SwapTarget {
                snapshot_name: "settings.json",
                live_path: paths::settings_json_path(&self.agent_dir),
                writer: Box::new(move |path| {
                    let _ = path;
                    settings::write_default(&dir_for_settings, &slot_for_settings, &model)
                }),
            });
        }
        let registry = self.registry.clone();
        let id = id.clone();
        swap_with_snapshot(PROVIDER_ID, &self.agent_dir, targets, move || {
            registry.set_active(PROVIDER_ID, &id)
        })
    }
}

#[async_trait]
impl EnvIsolatedProvider for PiProvider {
    fn provider_id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn native_cli(&self) -> &'static str {
        "pi"
    }

    /// 有原生 env 映射的 slot 直接注入进程环境；其余走文件切换，不支持进程隔离。
    async fn isolation_env(&self, id: &AccountId) -> Result<Option<Vec<(String, String)>>> {
        let provider = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            let account = provider.require_account(&id)?;
            let (api_key, slot, _) = provider.account_key(&account)?;
            let var = match slot.as_str() {
                "deepseek" => Some(DEEPSEEK_ENV_VAR),
                "kimi-coding" | "kimi" => Some(KIMI_ENV_VAR),
                _ => None,
            };
            Ok(var.map(|var| vec![(var.to_string(), api_key)]))
        })
        .await
        .map_err(|e| Error::Provider(format!("Pi env isolation join failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subswap_core::FileStore;

    struct Fixture {
        _tmp: tempfile::TempDir,
        provider: PiProvider,
        agent: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let agent = tmp.path().join("agent");
            let provider = PiProvider::with_agent_dir(
                Arc::new(FileStore::new(tmp.path().join("credentials.json"))),
                Arc::new(AccountRegistry::new(tmp.path().join("registry.toml"))),
                agent.clone(),
            );
            Self { _tmp: tmp, provider, agent }
        }

        fn deepseek_params() -> NewPiAccount {
            NewPiAccount {
                slot: "deepseek".into(),
                native_provider: "deepseek".into(),
                base_url: "https://api.deepseek.com/anthropic".into(),
                model: "deepseek-chat".into(),
                channel: "deepseek".into(),
                billing: BillingKind::Metered,
            }
        }
    }

    #[tokio::test]
    async fn auth_projection_is_long_lived_and_never_renews() {
        let fixture = Fixture::new();
        let account = fixture
            .provider
            .import_key(Fixture::deepseek_params(), "sk-long-lived", None)
            .unwrap();
        assert_eq!(account.provider, "pi");
        assert!(account.manual_only());
        assert!(!account.label.contains("sk-long-lived"));

        let credential = fixture
            .provider
            .request_credential(&account.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(credential.secret, "sk-long-lived");
        assert_eq!(credential.expires_at_ms, None);

        assert!(fixture.provider.renew_credential(&account.id).await.unwrap().is_none());
        // 再次投影仍是同一份长效 key（无续期副作用）。
        let again = fixture
            .provider
            .request_credential(&account.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again, credential);
    }

    #[tokio::test]
    async fn quota_is_always_empty_without_network() {
        let fixture = Fixture::new();
        let account = fixture
            .provider
            .import_key(Fixture::deepseek_params(), "sk-q", None)
            .unwrap();
        assert!(fixture.provider.query_quota(&account.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn swap_writes_slot_and_default_then_rolls_back_settings_failure() {
        let fixture = Fixture::new();
        auth::write_slot_merged(&fixture.agent, "other", "sk-neighbor").unwrap();
        let first = fixture
            .provider
            .import_key(Fixture::deepseek_params(), "sk-first", None)
            .unwrap();
        fixture.provider.activate(&first.id).await.unwrap();
        assert_eq!(
            auth::read_slot(&fixture.agent, "deepseek").unwrap().as_deref(),
            Some("sk-first")
        );
        assert_eq!(
            auth::read_slot(&fixture.agent, "other").unwrap().as_deref(),
            Some("sk-neighbor")
        );
        let (provider, model) = settings::read_default(&fixture.agent).unwrap();
        assert_eq!(provider.as_deref(), Some("deepseek"));
        assert_eq!(model.as_deref(), Some("deepseek-chat"));

        // settings.json 损坏时切换必须整体失败，且 auth.json 保持原样（快照回滚）。
        std::fs::write(paths::settings_json_path(&fixture.agent), "[broken").unwrap();
        let second = fixture
            .provider
            .import_key(Fixture::deepseek_params(), "sk-second", None)
            .unwrap();
        assert!(fixture.provider.activate(&second.id).await.is_err());
        assert_eq!(
            auth::read_slot(&fixture.agent, "deepseek").unwrap().as_deref(),
            Some("sk-first")
        );
        assert_eq!(
            auth::read_slot(&fixture.agent, "other").unwrap().as_deref(),
            Some("sk-neighbor")
        );
    }

    #[tokio::test]
    async fn swap_without_default_leaves_settings_alone() {
        let fixture = Fixture::new();
        let account = fixture
            .provider
            .import_key(Fixture::deepseek_params(), "sk-nodefault", None)
            .unwrap();
        fixture
            .provider
            .activate_with_default(&account.id, false)
            .await
            .unwrap();
        assert_eq!(
            auth::read_slot(&fixture.agent, "deepseek").unwrap().as_deref(),
            Some("sk-nodefault")
        );
        assert!(!paths::settings_json_path(&fixture.agent).exists());
    }

    #[tokio::test]
    async fn rm_clears_live_slot_but_never_refuses() {
        let fixture = Fixture::new();
        let account = fixture
            .provider
            .import_key(Fixture::deepseek_params(), "sk-rm", Some(true))
            .unwrap();
        fixture.provider.activate(&account.id).await.unwrap();
        let out = fixture.provider.disconnect_official(&account.id).await.unwrap();
        assert_eq!(out, OfficialDisconnect::Disconnected);
        assert!(auth::read_slot(&fixture.agent, "deepseek").unwrap().is_none());
        // parked / 已不在 live：幂等 AlreadyGone。
        assert_eq!(
            fixture.provider.disconnect_official(&account.id).await.unwrap(),
            OfficialDisconnect::AlreadyGone
        );
    }

    #[test]
    fn env_isolation_maps_known_slots_only() {
        let fixture = Fixture::new();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let deepseek = fixture.provider.import_key(Fixture::deepseek_params(), "sk-env", None).unwrap();
        let vars = rt.block_on(fixture.provider.isolation_env(&deepseek.id)).unwrap().unwrap();
        assert_eq!(vars, vec![("DEEPSEEK_API_KEY".to_string(), "sk-env".to_string())]);

        let volcengine = fixture
            .provider
            .import_key(
                NewPiAccount {
                    slot: "volcengine".into(),
                    native_provider: "volcengine".into(),
                    base_url: auth::PI_VOLCENGINE_BASE_URL.into(),
                    model: auth::PI_VOLCENGINE_MODEL.into(),
                    channel: auth::volcengine_channel(),
                    billing: BillingKind::Flat,
                },
                "sk-volc",
                None,
            )
            .unwrap();
        assert!(rt.block_on(fixture.provider.isolation_env(&volcengine.id)).unwrap().is_none());
    }
}

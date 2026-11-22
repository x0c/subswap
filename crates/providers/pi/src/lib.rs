//! Pi coding agent Runtime Provider：`~/.pi/agent/` 下多 slot API key 的唯一写主。
//!
//! - 每个账号占 Pi `auth.json` 里一个 slot（`auth_slot`，如 `deepseek`）；
//!   切换 = 原子改写该 slot（保留其他 slot）+ 可选更新 `settings.json` 默认选择。
//! - 账号全部 `manual_only`（不参与自动换号）；`query_quota` 按 `channel` 分发：
//!   标准火山 channel（`auth::volcengine_channel()`）的账号复用火山控制面 AK/SK
//!   直查（`GetCodingPlanUsage` / AFP + `arkcli` 兜底，与旧顶层 provider 同一路径），
//!   缺 AK/SK 时返回空不伪造；OpenCode Go channel（`opencode-go` / `opencode-go:*`）
//!   复用 `common::opencode_go_usage` 查询 `5h`/`7d`/`mo`；其余按量 channel 恒为空。
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
use subswap_provider_common::{opencode_go_usage, volcengine_quota, EnvIsolatedProvider};

pub use migration::{backfill_volcengine_quota_credentials, migrate_legacy_volcengine_to_pi};

/// 独立 Provider 标识。
pub const PROVIDER_ID: &str = "pi";
/// 凭证仓库字段：API key。
pub const API_KEY_FIELD: &str = "api_key";
/// 凭证仓库字段：额度查询用的火山 AK（与推理 Key 分开存放）。
pub const QUOTA_AK_FIELD: &str = "access_key_id";
/// 凭证仓库字段：额度查询用的火山 SK（与推理 Key 分开存放）。
pub const QUOTA_SK_FIELD: &str = "secret_access_key";

/// `deepseek` slot 的进程 env 映射（Pi 原生同样认它）。
const DEEPSEEK_ENV_VAR: &str = "DEEPSEEK_API_KEY";
/// `kimi-coding` slot 的进程 env 映射。
const KIMI_ENV_VAR: &str = "KIMI_API_KEY";
/// OpenCode Go / Zen 密钥在独立子进程里的 env 映射。
const OPENCODE_ENV_VAR: &str = "OPENCODE_API_KEY";

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
    /// 火山 channel 的额度 AK/SK（与推理 Key 分开存放）；非火山 channel 忽略。
    pub quota_ak: Option<String>,
    /// 火山 channel 的额度 SK；与 `quota_ak` 必须成对出现。
    pub quota_sk: Option<String>,
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

/// `add-api pi` 支持的预设：deepseek / volcengine / opencode-go / opencode-zen / custom。
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
        "opencode-go" | "opencode_go" => Some(PiPreset {
            slot: "opencode-go",
            native_provider: "opencode-go",
            endpoint: auth::PI_OPENCODE_GO_BASE_URL,
            model: auth::PI_OPENCODE_GO_MODEL,
            billing: BillingKind::Flat,
            env_var: Some(OPENCODE_ENV_VAR),
        }),
        "opencode" | "opencode-zen" | "opencode_zen" => Some(PiPreset {
            slot: "opencode-zen",
            native_provider: "opencode",
            endpoint: auth::PI_OPENCODE_ZEN_BASE_URL,
            model: auth::PI_OPENCODE_ZEN_MODEL,
            billing: BillingKind::Metered,
            env_var: Some(OPENCODE_ENV_VAR),
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
    pub fn import_key(
        &self,
        params: NewPiAccount,
        api_key: &str,
        active: Option<bool>,
    ) -> Result<Account> {
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
        // 火山 channel 的额度 AK/SK 与推理 Key 分开存放；成对出现才收（半边直接报错，不收半截）。
        match (params.quota_ak, params.quota_sk) {
            (None, None) => {}
            (Some(ak), Some(sk)) => {
                let ak = ak.trim().to_string();
                let sk = sk.trim().to_string();
                if ak.is_empty() || sk.is_empty() {
                    return Err(Error::Config(
                        "Volcengine AccessKey ID and Secret cannot be empty".into(),
                    ));
                }
                self.store.set(PROVIDER_ID, &id.0, QUOTA_AK_FIELD, &ak)?;
                self.store.set(PROVIDER_ID, &id.0, QUOTA_SK_FIELD, &sk)?;
            }
            _ => {
                return Err(Error::Config(
                    "quota_ak and quota_sk must be provided together".into(),
                ))
            }
        }
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

    /// 设置火山 channel 账号的额度 AK/SK（与推理 Key 分开存放）。
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
        self.require_account(id)?;
        self.store
            .set(PROVIDER_ID, &id.0, QUOTA_AK_FIELD, access_key_id)?;
        self.store
            .set(PROVIDER_ID, &id.0, QUOTA_SK_FIELD, secret_access_key)?;
        Ok(())
    }

    /// 读取火山额度 AK/SK；缺失返回 `None` 项，调用方直接按无额度处理（不造假）。
    fn quota_credentials(&self, id: &AccountId) -> Result<(Option<String>, Option<String>)> {
        let ak = self.store.get(PROVIDER_ID, &id.0, QUOTA_AK_FIELD)?;
        let sk = self.store.get(PROVIDER_ID, &id.0, QUOTA_SK_FIELD)?;
        let ak = ak
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let sk = sk
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Ok((ak, sk))
    }

    /// 是否 OpenCode Go 订阅 channel（支持 GET `/zen/go/v1/usage` 查 5h/7d/mo 余量）。
    fn is_opencode_go_channel(account: &Account) -> bool {
        account
            .extra
            .get("channel")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .is_some_and(|channel| {
                channel == auth::OPENCODE_GO_CHANNEL || channel.starts_with("opencode-go:")
            })
    }

    /// 是否标准火山 channel（额度 AK/SK 直查的唯一入口判据；按量 channel 永不查）。
    fn is_volcengine_channel(account: &Account) -> bool {
        account
            .extra
            .get("channel")
            .and_then(|v| v.as_str())
            .is_some_and(|channel| channel.trim() == auth::volcengine_channel())
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
        let preset_channel = auth::channel_for_slot(known.as_ref().map_or(slot, |p| p.slot));
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
                quota_ak: None,
                quota_sk: None,
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

    /// 标准火山与 OpenCode Go channel 各自直查余量；其余按量 channel 恒为空。
    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        self.query_quota_with_bases(id, None, None).await
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
        &[API_KEY_FIELD, QUOTA_AK_FIELD, QUOTA_SK_FIELD]
    }
}

/// Pi 账号的额度查询来源分发（内部辅助）。
enum PiQuotaSource {
    Empty,
    OpenCodeGo { api_key: String },
    Volcengine { ak: String, sk: String },
}

impl PiProvider {
    /// Query quota with optional per-request endpoints for local test gateways.
    async fn query_quota_with_bases(
        &self,
        id: &AccountId,
        opencode_go_base: Option<&str>,
        volcengine_base: Option<&str>,
    ) -> Result<Vec<Quota>> {
        let provider = self.clone();
        let id_for_lookup = id.clone();
        let source = tokio::task::spawn_blocking(move || -> Result<PiQuotaSource> {
            let account = provider.require_account(&id_for_lookup)?;
            if Self::is_opencode_go_channel(&account) {
                let (api_key, _, _) = provider.account_key(&account)?;
                return Ok(PiQuotaSource::OpenCodeGo { api_key });
            }
            if Self::is_volcengine_channel(&account) {
                let (ak, sk) = provider.quota_credentials(&id_for_lookup)?;
                if let (Some(ak), Some(sk)) = (ak, sk) {
                    return Ok(PiQuotaSource::Volcengine { ak, sk });
                }
            }
            Ok(PiQuotaSource::Empty)
        })
        .await
        .map_err(|e| Error::Provider(format!("Pi quota lookup join failed: {e}")))??;

        match source {
            PiQuotaSource::Empty => Ok(Vec::new()),
            PiQuotaSource::OpenCodeGo { api_key } => match opencode_go_base {
                Some(base) => {
                    opencode_go_usage::fetch_quota_at(&api_key, PROVIDER_ID, id, base).await
                }
                None => opencode_go_usage::fetch_quota(&api_key, PROVIDER_ID, id).await,
            },
            PiQuotaSource::Volcengine { ak, sk } => {
                volcengine_quota::fetch_quota_at(
                    PROVIDER_ID,
                    id.clone(),
                    Some(ak),
                    Some(sk),
                    volcengine_base,
                )
                .await
            }
        }
    }

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
                "opencode" | "opencode-go" | "opencode-zen" => Some(OPENCODE_ENV_VAR),
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
            Self {
                _tmp: tmp,
                provider,
                agent,
            }
        }

        fn deepseek_params() -> NewPiAccount {
            NewPiAccount {
                slot: "deepseek".into(),
                native_provider: "deepseek".into(),
                base_url: "https://api.deepseek.com/anthropic".into(),
                model: "deepseek-chat".into(),
                channel: "deepseek".into(),
                billing: BillingKind::Metered,
                quota_ak: None,
                quota_sk: None,
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

        assert!(fixture
            .provider
            .renew_credential(&account.id)
            .await
            .unwrap()
            .is_none());
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
        assert!(fixture
            .provider
            .query_quota(&account.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn volcengine_slot_without_aksk_returns_empty_without_network() {
        let fixture = Fixture::new();
        let account = fixture
            .provider
            .import_key(
                NewPiAccount {
                    slot: "volcengine".into(),
                    native_provider: "volcengine".into(),
                    base_url: auth::PI_VOLCENGINE_BASE_URL.into(),
                    model: auth::PI_VOLCENGINE_MODEL.into(),
                    channel: auth::volcengine_channel(),
                    billing: BillingKind::Flat,
                    quota_ak: None,
                    quota_sk: None,
                },
                "sk-volc-noquota",
                None,
            )
            .unwrap();
        // 缺 AK/SK：不打网，直接空额度（不造假）。
        assert!(fixture
            .provider
            .query_quota(&account.id)
            .await
            .unwrap()
            .is_empty());
        // 配上 AK/SK 后走同一火山直查路径（此处不断言远端结果，只验字段落盘）。
        fixture
            .provider
            .set_quota_credentials(&account.id, "AK", "SK")
            .unwrap();
        let store = fixture.provider.store.clone();
        let id = account.id.clone();
        let (ak, sk) = tokio::task::spawn_blocking(move || {
            let ak = store.get("pi", &id.0, super::QUOTA_AK_FIELD).unwrap();
            let sk = store.get("pi", &id.0, super::QUOTA_SK_FIELD).unwrap();
            (ak, sk)
        })
        .await
        .unwrap();
        assert_eq!(ak.as_deref(), Some("AK"));
        assert_eq!(sk.as_deref(), Some("SK"));
    }

    fn volcengine_params() -> NewPiAccount {
        NewPiAccount {
            slot: "volcengine".into(),
            native_provider: "volcengine".into(),
            base_url: auth::PI_VOLCENGINE_BASE_URL.into(),
            model: auth::PI_VOLCENGINE_MODEL.into(),
            channel: auth::volcengine_channel(),
            billing: BillingKind::Flat,
            quota_ak: Some("AK-TEST".into()),
            quota_sk: Some("SK-TEST".into()),
        }
    }

    /// 起最小 HTTP mock：按顺序返回给定 payload；返回 base url 与收到的请求原文。
    fn spawn_json_mock(payloads: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for payload in payloads {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let mut header_end = None;
                while header_end.is_none() {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            raw.extend_from_slice(&buf[..n]);
                            if let Some(pos) = find_mock_headers_end(&raw) {
                                header_end = Some(pos);
                                break;
                            }
                            if raw.len() > 32 * 1024 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let header_end = header_end.unwrap_or(raw.len());
                let header_text = String::from_utf8_lossy(&raw[..header_end]).to_string();
                let content_len = header_text
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let mut body_len = raw.len().saturating_sub(header_end);
                while body_len < content_len {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            raw.extend_from_slice(&buf[..n]);
                            body_len += n;
                        }
                        Err(_) => break,
                    }
                }
                seen.push(String::from_utf8_lossy(&raw).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = stream.write_all(response.as_bytes());
            }
            seen
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn find_mock_headers_end(raw: &[u8]) -> Option<usize> {
        raw.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|pos| pos + 4)
    }

    /// 火山 channel + AK/SK：mock 网关返回 5h/7d/mo 三档百分比（CodingPlan 优先，只打一次）。
    #[tokio::test]
    async fn volcengine_channel_with_aksk_reports_three_windows() {
        let payload = r#"{"ResponseMetadata":{},"Result":{"Status":"Running","QuotaUsage":[{"Level":"session","Percent":10.5,"ResetTimestamp":1784278625},{"Level":"weekly","Percent":43.5,"ResetTimestamp":1784217600},{"Level":"monthly","Percent":21.7,"ResetTimestamp":1786723199}]}}"#.to_string();
        let (base, server) = spawn_json_mock(vec![payload]);

        let fixture = Fixture::new();
        let account = fixture
            .provider
            .import_key(volcengine_params(), "sk-volc-quota", None)
            .unwrap();
        let quotas = fixture
            .provider
            .query_quota_with_bases(&account.id, None, Some(&base))
            .await
            .unwrap();
        let seen = server.join().unwrap();

        assert_eq!(quotas.len(), 3);
        assert!(quotas.iter().all(|q| q.provider == "pi"));
        assert_eq!(quotas[0].window, subswap_core::QuotaWindow::FiveHour);
        assert_eq!((quotas[0].used, quotas[0].limit), (11, 100));
        assert_eq!(quotas[1].window, subswap_core::QuotaWindow::SevenDay);
        assert_eq!(quotas[2].window, subswap_core::QuotaWindow::Month);
        // CodingPlan 命中后不再打 AFP。
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("Action=GetCodingPlanUsage"));
    }

    /// 鉴权失败时错误与外发请求均不得携带 SK 原文（AK 只出现在签名 Credential 作用域，属协议行为）。
    #[tokio::test]
    async fn quota_auth_failure_never_echoes_secret() {
        let secret = "synthetic-secret-value-must-not-leak";
        let access_id = "AKID-MUST-NOT-LEAK-12345";
        let payload = r#"{"ResponseMetadata":{"Error":{"Code":"InvalidAccessKeyId","Message":"ak is wrong"}},"Result":{}}"#.to_string();
        // CodingPlan 与 AFP 各打一次，共两个连接。
        let (base, server) = spawn_json_mock(vec![payload.clone(), payload]);

        let fixture = Fixture::new();
        let account = fixture
            .provider
            .import_key(
                NewPiAccount {
                    quota_ak: Some(access_id.into()),
                    quota_sk: Some(secret.into()),
                    ..volcengine_params()
                },
                "sk-volc-badauth",
                None,
            )
            .unwrap();
        let err = fixture
            .provider
            .query_quota_with_bases(&account.id, None, Some(&base))
            .await
            .unwrap_err();
        let seen = server.join().unwrap();

        let message = err.to_string();
        assert!(!message.contains(secret), "error leaks SK: {message}");
        assert!(!message.contains(access_id), "error leaks AK: {message}");
        assert_eq!(seen.len(), 2);
        for req in &seen {
            assert!(!req.contains(secret), "request leaks SK: {req}");
        }
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
            auth::read_slot(&fixture.agent, "deepseek")
                .unwrap()
                .as_deref(),
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
            auth::read_slot(&fixture.agent, "deepseek")
                .unwrap()
                .as_deref(),
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
            auth::read_slot(&fixture.agent, "deepseek")
                .unwrap()
                .as_deref(),
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
        let out = fixture
            .provider
            .disconnect_official(&account.id)
            .await
            .unwrap();
        assert_eq!(out, OfficialDisconnect::Disconnected);
        assert!(auth::read_slot(&fixture.agent, "deepseek")
            .unwrap()
            .is_none());
        // parked / 已不在 live：幂等 AlreadyGone。
        assert_eq!(
            fixture
                .provider
                .disconnect_official(&account.id)
                .await
                .unwrap(),
            OfficialDisconnect::AlreadyGone
        );
    }

    #[test]
    fn env_isolation_maps_known_slots_only() {
        let fixture = Fixture::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let deepseek = fixture
            .provider
            .import_key(Fixture::deepseek_params(), "sk-env", None)
            .unwrap();
        let vars = rt
            .block_on(fixture.provider.isolation_env(&deepseek.id))
            .unwrap()
            .unwrap();
        assert_eq!(
            vars,
            vec![("DEEPSEEK_API_KEY".to_string(), "sk-env".to_string())]
        );

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
                    quota_ak: None,
                    quota_sk: None,
                },
                "sk-volc",
                None,
            )
            .unwrap();
        assert!(rt
            .block_on(fixture.provider.isolation_env(&volcengine.id))
            .unwrap()
            .is_none());
    }

    #[test]
    fn opencode_presets_and_isolation_env_match_expected_channels() {
        let fixture = Fixture::new();
        let go_preset = preset("opencode-go").expect("opencode-go preset");
        assert_eq!(go_preset.slot, "opencode-go");
        assert_eq!(go_preset.native_provider, "opencode-go");
        assert_eq!(go_preset.endpoint, auth::PI_OPENCODE_GO_BASE_URL);
        assert_eq!(go_preset.billing, BillingKind::Flat);
        assert_eq!(go_preset.env_var, Some("OPENCODE_API_KEY"));

        let zen_preset = preset("opencode-zen").expect("opencode-zen preset");
        assert_eq!(zen_preset.slot, "opencode-zen");
        assert_eq!(zen_preset.native_provider, "opencode");
        assert_eq!(zen_preset.endpoint, auth::PI_OPENCODE_ZEN_BASE_URL);
        assert_eq!(zen_preset.billing, BillingKind::Metered);
        assert_eq!(zen_preset.env_var, Some("OPENCODE_API_KEY"));
        assert_eq!(preset("opencode").unwrap().slot, "opencode-zen");

        let imported = fixture
            .provider
            .import_key(
                NewPiAccount {
                    slot: go_preset.slot.to_string(),
                    native_provider: go_preset.native_provider.to_string(),
                    base_url: go_preset.endpoint.to_string(),
                    model: go_preset.model.to_string(),
                    channel: auth::channel_for_slot(go_preset.slot),
                    ..Fixture::deepseek_params()
                },
                "oc_sk_000000000000_test8888",
                None,
            )
            .unwrap();
        assert_eq!(
            imported.extra.get("channel").and_then(|v| v.as_str()),
            Some("opencode-go")
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let env = rt
            .block_on(fixture.provider.isolation_env(&imported.id))
            .unwrap()
            .expect("opencode-go should support process isolation");
        assert_eq!(
            env,
            vec![(
                "OPENCODE_API_KEY".to_string(),
                "oc_sk_000000000000_test8888".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn opencode_go_channel_queries_usage_while_zen_stays_empty() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use subswap_core::QuotaWindow;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen_bg = std::sync::Arc::clone(&seen);
        let handle = std::thread::spawn(move || {
            let responses = [
                (
                    "200 OK",
                    r#"{"usage":{"rolling":{"status":"ok","percent":12,"resetsAt":"2026-11-15T05:00:00Z"},"weekly":{"status":"ok","percent":8,"resetsAt":"2026-11-20T00:00:00Z"},"monthly":{"status":"ok","percent":2,"resetsAt":"2026-12-01T00:00:00Z"}}}"#,
                ),
                ("429 Too Many Requests", r#"{"error":"rate_limited"}"#),
            ];
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                seen_bg
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });

        let fixture = Fixture::new();
        let go_preset = preset("opencode-go").unwrap();
        let go_account = fixture
            .provider
            .import_key(
                NewPiAccount {
                    slot: go_preset.slot.to_string(),
                    native_provider: go_preset.native_provider.to_string(),
                    base_url: go_preset.endpoint.to_string(),
                    model: go_preset.model.to_string(),
                    channel: auth::channel_for_slot(go_preset.slot),
                    billing: go_preset.billing,
                    quota_ak: None,
                    quota_sk: None,
                },
                "oc_sk_000000000000_testgo11",
                None,
            )
            .unwrap();

        let zen_preset = preset("opencode-zen").unwrap();
        let zen_account = fixture
            .provider
            .import_key(
                NewPiAccount {
                    slot: zen_preset.slot.to_string(),
                    native_provider: zen_preset.native_provider.to_string(),
                    base_url: zen_preset.endpoint.to_string(),
                    model: zen_preset.model.to_string(),
                    channel: auth::channel_for_slot(zen_preset.slot),
                    billing: zen_preset.billing,
                    quota_ak: None,
                    quota_sk: None,
                },
                "oc_sk_000000000000_testzen2",
                None,
            )
            .unwrap();

        // opencode-zen 按量无 Go 订阅额度，不发 HTTP 请求直接返回空。
        let zen_quotas = fixture
            .provider
            .query_quota_with_bases(&zen_account.id, Some(&base_url), None)
            .await
            .unwrap();
        assert!(zen_quotas.is_empty());

        // opencode-go 返回 rolling / weekly / monthly 三个窗口。
        let go_quotas = fixture
            .provider
            .query_quota_with_bases(&go_account.id, Some(&base_url), None)
            .await
            .unwrap();
        assert_eq!(go_quotas.len(), 3);
        assert_eq!(go_quotas[0].provider, "pi");
        assert_eq!(go_quotas[0].window, QuotaWindow::FiveHour);
        assert_eq!(go_quotas[0].used, 12);
        assert_eq!(go_quotas[1].window, QuotaWindow::SevenDay);
        assert_eq!(go_quotas[1].used, 8);
        assert_eq!(go_quotas[2].window, QuotaWindow::Month);
        assert_eq!(go_quotas[2].used, 2);

        // 429 保持 typed RateLimited，不当成身份失效。
        let err = fixture
            .provider
            .query_quota_with_bases(&go_account.id, Some(&base_url), None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::QuotaRateLimited { .. }), "got {err}");
        assert!(!subswap_core::is_authentication_failure(&err.to_string()));

        handle.join().unwrap();
        let captured = seen.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert!(captured[0].starts_with("GET /usage HTTP/1.1"));
        assert!(captured[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer oc_sk_000000000000_testgo11"));
    }
}

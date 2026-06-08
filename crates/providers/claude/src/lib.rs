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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use subswap_core::error::{Error, Result};
use subswap_core::swap::{swap_with_snapshot, SwapTarget};
use subswap_core::{
    Account, AccountId, AccountRegistry, ClientTarget, CredentialStore, Provider, Quota,
    QuotaStatus, QuotaWindow,
};

use crate::claude_files::{
    read_credentials, read_oauth_account, write_credentials, write_oauth_account_into_global,
    CredentialsFile, OauthAccount,
};
use crate::paths::{claude_home, credentials_path, global_config_path};

/// 凭证字段名：整段 credentials.json 的 JSON 序列化结果。
const CRED_FIELD: &str = "credentials_json";
/// Provider 标识。
pub const PROVIDER_ID: &str = "claude";
// 数值调优参数运行时取自 [`subswap_core::settings::current`]；config.toml 即时生效。
use subswap_core::settings;

pub struct ClaudeProvider {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    claude_home: PathBuf,
}

impl ClaudeProvider {
    pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> Self {
        Self {
            store,
            registry,
            claude_home: claude_home(),
        }
    }

    /// 把当前 `~/.claude` 下激活的账号导入为 subswap 管理的账号。
    pub fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
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
        if creds.oauth.refresh_token.is_none() {
            return Ok(false);
        }
        apply_refresh_to_creds(&mut creds).await?;
        self.save_credentials(id, &creds)?;
        Ok(true)
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

    /// 读取当前激活账号的实时凭证：实体文件优先，macOS 上回落 Claude Code 钥匙串 item。
    /// 动机：macOS 上 Claude Code 把凭证写进钥匙串、不写 `~/.claude/.credentials.json`。
    fn read_live_credentials(&self) -> Result<CredentialsFile> {
        match read_credentials(&credentials_path(&self.claude_home)) {
            Ok(creds) => Ok(creds),
            Err(file_err) => read_claude_code_keychain().ok_or(file_err),
        }
    }

    /// 当前 Claude Code 激活账号的 id(取自 `~/.claude.json` 的 `oauthAccount`)。不读 keyring。
    /// 没有 oauthAccount(未登录)时返回 `None`。daemon 保活与 quota 自愈用它跳过 active 账号——
    /// active 账号的 token 由 Claude Code 唯一轮换,subswap 不得在后台抢刷。
    fn active_account_id(&self) -> Result<Option<AccountId>> {
        let Some(oauth_account) = read_oauth_account(&global_config_path(&self.claude_home))? else {
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
        let Some(oauth_account) = read_oauth_account(&global_config_path(&self.claude_home))? else {
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
        let claude_home = self.claude_home.clone();
        let registry = self.registry.clone();
        let store = self.store.clone();
        let id_for_blocking = id.clone();

        tokio::task::spawn_blocking(move || {
            // capture-on-leave：覆盖 live 文件前，把当前 live 凭证回灌进它所属账号的 store。
            // 否则切走的账号 store 副本会停在旧 refresh token，下次切回写回旧 token → "already used"。
            if let Err(e) = capture_live_into_store(store.as_ref(), &registry, &claude_home) {
                tracing::warn!(err = %e, "claude capture-on-leave failed; continuing swap");
            }

            let targets = vec![
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
            let result = swap_with_snapshot(PROVIDER_ID, &claude_home, targets, || {
                registry.set_active(PROVIDER_ID, &id_for_blocking)
            });
            if result.is_ok() {
                tracing::info!(account = %id_for_blocking, "Claude swap done");
            }
            result
        })
        .await
        .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))?
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
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
                apply_refresh_to_creds(&mut creds).await?;
                // 刷新后的 token 写回凭证仓库,避免下次查询重复刷新(FileStore 写入不弹钥匙串)。
                self.save_credentials(id, &creds)?;
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

impl ClaudeProvider {
    fn read_active_credentials_if_matches(
        &self,
        id: &AccountId,
    ) -> Result<Option<CredentialsFile>> {
        let creds = match read_credentials(&credentials_path(&self.claude_home)) {
            Ok(creds) => creds,
            Err(_) => return Ok(None),
        };
        let Some(oauth_account) = read_oauth_account(&global_config_path(&self.claude_home))?
        else {
            return Ok(None);
        };
        if oauth_account.email_address == id.0 {
            return Ok(Some(creds));
        }
        Ok(None)
    }
}

/// macOS：读 Claude Code 的系统钥匙串 generic password —— `service = "Claude Code-credentials"`,
/// `account = <登录用户名>`,内容与 `.credentials.json` 同构(`{"claudeAiOauth": {...}}`)。
/// 这是 macOS 上 claude 凭证的唯一来源。读不到(不存在 / 用户拒绝授权 / 解析失败)一律返回 `None`。
#[cfg(target_os = "macos")]
fn read_claude_code_keychain() -> Option<CredentialsFile> {
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty())?;
    let entry = keyring::Entry::new("Claude Code-credentials", &user).ok()?;
    let raw = entry.get_password().ok()?;
    serde_json::from_str::<CredentialsFile>(&raw).ok()
}

/// 非 macOS：凭证走实体文件,无此回落。
#[cfg(not(target_os = "macos"))]
fn read_claude_code_keychain() -> Option<CredentialsFile> {
    None
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
) -> Result<()> {
    // live 凭证：实体文件优先,失败回落 macOS Claude Code 钥匙串。
    let live_creds = match read_credentials(&credentials_path(claude_home)) {
        Ok(creds) => creds,
        Err(_) => match read_claude_code_keychain() {
            Some(creds) => creds,
            None => return Ok(()),
        },
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
    if creds.oauth.refresh_token.is_none() {
        tracing::warn!(
            "token expired/expiring but no refreshToken in keyring; skipping pre-refresh — log in again if the client returns 401"
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
    let refresh_token = creds.oauth.refresh_token.clone().ok_or_else(|| {
        Error::Provider(format!(
            "{PROVIDER_ID} account has no refreshToken; cannot refresh offline, log in and re-add"
        ))
    })?;
    let resp = oauth::refresh_access_token(&refresh_token).await?;
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

        capture_live_into_store(&store, &registry, &home).unwrap();

        // 回灌后 store 应反映 live 的 R2。
        let stored = store.get(PROVIDER_ID, "a@x.com", CRED_FIELD).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(v["claudeAiOauth"]["refreshToken"], "R2");
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
        capture_live_into_store(&store, &registry, &home).unwrap();
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
        };
        assert_eq!(
            provider.active_account_id().unwrap(),
            Some(AccountId("active@x.com".into()))
        );
    }
}

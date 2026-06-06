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

use std::path::PathBuf;
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
        let creds_path = credentials_path(&self.claude_home);
        let creds = read_credentials(&creds_path)?;
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

    /// 从 keyring 读 `~/.claude/.credentials.json` 的 JSON 副本。
    fn load_credentials(&self, id: &AccountId) -> Result<CredentialsFile> {
        let raw = self
            .store
            .get(PROVIDER_ID, id.0.as_str(), CRED_FIELD)?
            .ok_or_else(|| {
                Error::Credential(format!(
                    "no keyring entry for {PROVIDER_ID}:{id}:{CRED_FIELD}"
                ))
            })?;
        Ok(serde_json::from_str(&raw)?)
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
        let id_for_blocking = id.clone();

        tokio::task::spawn_blocking(move || {
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
        let mut creds = match self.read_active_credentials_if_matches(id)? {
            Some(creds) => creds,
            None if !quota_keychain_access_enabled() => {
                return Err(Error::Credential(
                    "quota skipped on macOS; set SUBSWAP_QUERY_INACTIVE_KEYCHAIN=1 to allow keychain-backed quota"
                        .into(),
                ));
            }
            None => self.load_credentials(id)?,
        };
        // 进程内自愈：access_token 失效(401)且有 refresh_token 时，best-effort 刷新一次再重试。
        // 动机：daemon 后台保活在部分环境(如 Linux keyutils 按 session 隔离)读不到本进程写入的
        //       keyring 条目，无法保活；查询进程能看到自己的 keyring，因此在这里自愈最可靠。
        // 保守起见只在 401 时刷新、且只重试一次，避免请求风暴(AGENTS.md #10)。
        let usage = match oauth::fetch_usage(&creds.oauth.access_token).await {
            Ok(u) => u,
            Err(e) if is_auth_error(&e) && creds.oauth.refresh_token.is_some() => {
                apply_refresh_to_creds(&mut creds).await?;
                if keychain_write_back_enabled() {
                    self.save_credentials(id, &creds)?;
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

#[cfg(target_os = "macos")]
fn keychain_write_back_enabled() -> bool {
    std::env::var_os("SUBSWAP_SYNC_KEYCHAIN_ON_START").is_some()
}

#[cfg(not(target_os = "macos"))]
fn keychain_write_back_enabled() -> bool {
    true
}

#[cfg(target_os = "macos")]
fn quota_keychain_access_enabled() -> bool {
    std::env::var_os("SUBSWAP_QUERY_INACTIVE_KEYCHAIN").is_some()
}

#[cfg(not(target_os = "macos"))]
fn quota_keychain_access_enabled() -> bool {
    true
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
}

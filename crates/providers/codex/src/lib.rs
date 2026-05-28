//! Codex / ChatGPT Provider 实现。
//!
//! 关键约束：
//! - `activate` **不依赖** `query_quota`：网络不通也能切换。
//! - 整段 `~/.codex/auth.json` 作为 opaque blob 进 keyring，subswap 不假设 schema 稳定。
//! - 切换 = flock → snapshot 旧文件 → 原子写新 auth.json → 任一步失败回滚。

mod codex_files;
mod openai_usage;
mod paths;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use fs2::FileExt;

use subswap_core::defaults;
use subswap_core::error::{Error, Result};
use subswap_core::{
    Account, AccountId, AccountRegistry, ClientTarget, CredentialStore, Provider, Quota,
    QuotaStatus, QuotaWindow,
};

use crate::codex_files::{parse_metadata, read_auth, write_auth, AuthMetadata};
use crate::paths::{active_auth_path, codex_home};

/// keyring 字段名。
const AUTH_FIELD: &str = "auth_json";
/// registry.toml `extra.chatgpt_account_id` 字段名，用于额度查询时拼 header。
const META_CHATGPT_ACCOUNT_ID: &str = "chatgpt_account_id";
/// registry.toml `extra.metadata` 字段名，存 [`AuthMetadata`] 全量供 list 展示。
const META_AUTH_METADATA: &str = "auth_metadata";

pub const PROVIDER_ID: &str = "codex";

pub struct CodexProvider {
    store: Arc<dyn CredentialStore>,
    registry: Arc<AccountRegistry>,
    codex_home: PathBuf,
}

impl CodexProvider {
    pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> Self {
        Self {
            store,
            registry,
            codex_home: codex_home(),
        }
    }

    /// 从当前 `~/.codex/auth.json` 导入。
    pub fn import_active(&self, label_hint: Option<String>) -> Result<Account> {
        let raw = read_auth(&active_auth_path(&self.codex_home))?;
        self.store_account(raw, label_hint)
    }

    /// 从指定 auth.json 文件导入。
    pub fn import_from_file(
        &self,
        auth_file: PathBuf,
        label_hint: Option<String>,
    ) -> Result<Account> {
        let raw = std::fs::read_to_string(&auth_file)?;
        // 至少验证是合法 JSON，否则后续切换写入就麻烦。
        serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| {
            Error::Provider(format!("{} is not valid JSON: {e}", auth_file.display()))
        })?;
        self.store_account(raw, label_hint)
    }

    /// 从 codex-auth 的 registry 元数据 + opaque auth blob 导入账号。
    pub fn import_raw_with_metadata(
        &self,
        raw_auth_json: String,
        metadata_json: serde_json::Value,
        active: bool,
    ) -> Result<Account> {
        let metadata: AuthMetadata = serde_json::from_value(metadata_json)
            .unwrap_or_else(|_| parse_metadata(&raw_auth_json));
        self.store_account_with_metadata(raw_auth_json, metadata, None, Some(active))
    }

    fn store_account(&self, raw_auth_json: String, label_hint: Option<String>) -> Result<Account> {
        let metadata = parse_metadata(&raw_auth_json);
        self.store_account_with_metadata(raw_auth_json, metadata, label_hint, None)
    }

    fn store_account_with_metadata(
        &self,
        raw_auth_json: String,
        metadata: AuthMetadata,
        label_hint: Option<String>,
        active_override: Option<bool>,
    ) -> Result<Account> {
        let id_string = metadata
            .primary_id()
            .or_else(|| label_hint.clone())
            .ok_or_else(|| {
                Error::Provider(
                    "cannot parse account_key/email from auth.json; pass --label to set it explicitly".into(),
                )
            })?;
        let id = AccountId(id_string);
        let label = label_hint
            .or_else(|| metadata.label())
            .unwrap_or_else(|| id.0.clone());

        // 1. blob 进 keyring。
        self.store
            .set(PROVIDER_ID, id.0.as_str(), AUTH_FIELD, &raw_auth_json)?;

        // 2. 元数据进 registry.toml。
        let mut extra = serde_json::Map::new();
        extra.insert(META_AUTH_METADATA.into(), serde_json::to_value(&metadata)?);
        if let Some(cid) = metadata.chatgpt_account_id.clone() {
            extra.insert(
                META_CHATGPT_ACCOUNT_ID.into(),
                serde_json::Value::String(cid),
            );
        }

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
impl Provider for CodexProvider {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn display_name(&self) -> &'static str {
        "Codex / ChatGPT"
    }

    fn client_targets(&self) -> Vec<ClientTarget> {
        // 实际上 Codex CLI / VSCode 扩展 / Codex App 都从 ~/.codex/auth.json 读取；
        // 切换只需要写这一个文件即可同步三端。
        vec![ClientTarget {
            id: "codex_auth".into(),
            display_name: "Codex auth file".into(),
            probe_path: active_auth_path(&self.codex_home),
        }]
    }

    async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.registry.list_by_provider(PROVIDER_ID)
    }

    async fn activate(&self, id: &AccountId) -> Result<()> {
        // 阶段 1：异步预处理（仅查 registry + keyring，无网络调用）。
        let _account =
            self.registry
                .find(PROVIDER_ID, id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                })?;
        let target_raw = self
            .store
            .get(PROVIDER_ID, id.0.as_str(), AUTH_FIELD)?
            .ok_or_else(|| {
                Error::Credential(format!(
                    "no keyring entry for {PROVIDER_ID}:{id}:{AUTH_FIELD}; re-add this account"
                ))
            })?;

        // 阶段 2：同步阻塞部分搬进 spawn_blocking，避免堵塞 tokio worker。
        let codex_home = self.codex_home.clone();
        let auth_path = active_auth_path(&self.codex_home);
        let registry = self.registry.clone();
        let id_for_blocking = id.clone();

        tokio::task::spawn_blocking(move || {
            activate_files_sync(
                &codex_home,
                &auth_path,
                &target_raw,
                &registry,
                &id_for_blocking,
            )
        })
        .await
        .map_err(|e| Error::Provider(format!("spawn_blocking join failed: {e}")))?
    }

    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>> {
        // 1. 拿元数据里的 chatgpt_account_id。
        let account =
            self.registry
                .find(PROVIDER_ID, id)?
                .ok_or_else(|| Error::AccountNotFound {
                    provider: PROVIDER_ID.into(),
                    id: id.to_string(),
                })?;
        let chatgpt_account_id = account
            .extra
            .get(META_CHATGPT_ACCOUNT_ID)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::QuotaFetch(format!(
                    "registry entry {PROVIDER_ID}:{id} missing {META_CHATGPT_ACCOUNT_ID}; cannot fetch usage"
                ))
            })?
            .to_string();

        // 2. 从 auth blob 抽 access_token。键名在 codex 内通常嵌在 tokens.access_token 下；
        //    schema 不稳定，所以做一次松散查找。
        let raw = self
            .store
            .get(PROVIDER_ID, id.0.as_str(), AUTH_FIELD)?
            .ok_or_else(|| {
                Error::Credential(format!(
                    "no keyring entry for {PROVIDER_ID}:{id}:{AUTH_FIELD}"
                ))
            })?;
        let access_token = extract_access_token(&raw).ok_or_else(|| {
            Error::QuotaFetch(
                "no access_token in auth.json; codex may have changed its schema — subswap parser needs an update"
                    .into(),
            )
        })?;

        // 3. 调端点。
        let raw_resp = openai_usage::fetch_usage_raw(&access_token, &chatgpt_account_id).await?;
        let mut normalized = openai_usage::normalize_all(&raw_resp);
        if normalized.iter().all(usage_has_unknown_quota) {
            tracing::debug!(
                account=%id,
                shape=%openai_usage::shape_summary(&raw_resp),
                "wham/usage fields unrecognized"
            );
            if let Some(cached_usage) = fresh_cached_codex_auth_usage(&account) {
                tracing::debug!(
                    account=%id,
                    "using fresh codex-auth usage cache because wham/usage fields were unrecognized"
                );
                normalized = openai_usage::normalize_all(&cached_usage);
            }
        }

        Ok(normalized
            .into_iter()
            .map(|norm| {
                let percent = norm.used_percent.or(norm.percent);
                let reset_at = norm.resets_at.or(norm.reset_at).map(epoch_to_datetime);

                let (used, limit, status) = match (percent, norm.used, norm.limit) {
                    (Some(pct), _, _) => {
                        let used = pct.round().clamp(0.0, 100.0) as u64;
                        let status = if pct >= defaults::QUOTA_EXHAUSTED_PCT {
                            QuotaStatus::Exhausted
                        } else if pct >= defaults::QUOTA_WARN_PCT {
                            QuotaStatus::Warn
                        } else {
                            QuotaStatus::Ok
                        };
                        (used, 100, status)
                    }
                    (None, Some(u), Some(l)) if l > 0 => {
                        let pct = (u as f64 / l as f64) * 100.0;
                        let status = if pct >= defaults::QUOTA_EXHAUSTED_PCT {
                            QuotaStatus::Exhausted
                        } else if pct >= defaults::QUOTA_WARN_PCT {
                            QuotaStatus::Warn
                        } else {
                            QuotaStatus::Ok
                        };
                        (u, l, status)
                    }
                    _ => (0, 0, QuotaStatus::Unknown),
                };

                Quota {
                    provider: PROVIDER_ID.into(),
                    account_id: id.clone(),
                    window: quota_window_for_usage_window(
                        norm.window_minutes,
                        norm.limit_window_seconds,
                    ),
                    used,
                    limit,
                    reset_at,
                    status,
                    note: if matches!(status, QuotaStatus::Unknown) {
                        Some("wham/usage fields unrecognized".into())
                    } else {
                        None
                    },
                }
            })
            .collect())
    }
}

fn usage_has_unknown_quota(usage: &openai_usage::WhamUsage) -> bool {
    usage.used_percent.is_none()
        && usage.percent.is_none()
        && !matches!((usage.used, usage.limit), (Some(_), Some(limit)) if limit > 0)
}

fn fresh_cached_codex_auth_usage(account: &Account) -> Option<serde_json::Value> {
    let metadata = account.extra.get(META_AUTH_METADATA)?;
    let usage = metadata.get("last_usage")?.clone();
    let cached_at = metadata.get("last_usage_at").and_then(|v| v.as_i64())?;
    let cached_at_ms = epoch_to_millis(cached_at);
    let age_ms = Utc::now().timestamp_millis().saturating_sub(cached_at_ms);
    (age_ms <= defaults::CODEX_USAGE_CACHE_MAX_AGE_MS).then_some(usage)
}

fn quota_window_for_usage_window(minutes: Option<u64>, seconds: Option<u64>) -> QuotaWindow {
    match minutes.or_else(|| seconds.map(|value| value / 60)) {
        Some(300) => QuotaWindow::FiveHour,
        Some(10_080) => QuotaWindow::SevenDay,
        _ => QuotaWindow::Custom,
    }
}

fn epoch_to_millis(epoch: i64) -> i64 {
    if epoch.abs() > 1_000_000_000_000 {
        epoch
    } else {
        epoch.saturating_mul(1000)
    }
}

/// 同步部分：在 spawn_blocking 内执行。所有 IO 都在这里。
fn activate_files_sync(
    codex_home: &Path,
    auth_path: &Path,
    target_raw: &str,
    registry: &AccountRegistry,
    id: &AccountId,
) -> Result<()> {
    // 文件锁。
    std::fs::create_dir_all(codex_home)?;
    let lock_path = codex_home.join(".subswap.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file
        .lock_exclusive()
        .map_err(|e| Error::Provider(format!("lock {} failed: {e}", lock_path.display())))?;

    // snapshot。
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let snap_dir = subswap_core::paths::AppPaths::resolve()?
        .snapshots_dir()
        .join(format!("codex-{ts}"));
    std::fs::create_dir_all(&snap_dir)?;

    let backup = if auth_path.exists() {
        let raw = std::fs::read_to_string(auth_path)?;
        std::fs::write(snap_dir.join("auth.json"), &raw)?;
        Some(raw)
    } else {
        None
    };
    tracing::info!(snapshot = %snap_dir.display(), "pre-swap snapshot saved");

    if let Err(e) = write_auth(auth_path, target_raw) {
        rollback_auth(auth_path, &backup);
        let _ = FileExt::unlock(&lock_file);
        return Err(e);
    }
    if let Err(e) = registry.set_active(PROVIDER_ID, id) {
        rollback_auth(auth_path, &backup);
        let _ = FileExt::unlock(&lock_file);
        return Err(e);
    }
    let _ = FileExt::unlock(&lock_file);
    tracing::info!(account = %id, "Codex swap done");
    Ok(())
}

fn rollback_auth(auth_path: &Path, backup: &Option<String>) {
    if let Some(raw) = backup {
        if let Err(e) = std::fs::write(auth_path, raw) {
            tracing::error!(err=%e, path=%auth_path.display(), "rollback auth.json failed");
        }
    } else {
        let _ = std::fs::remove_file(auth_path);
    }
}

/// 在 codex auth.json 这种半结构化 JSON 中宽松查找 access_token。
/// 兼容 `{"tokens":{"access_token":"..."}}`、`{"access_token":"..."}` 等几种常见写法。
fn extract_access_token(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    fn walk(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(s)) = map.get("access_token") {
                    return Some(s.clone());
                }
                for child in map.values() {
                    if let Some(found) = walk(child) {
                        return Some(found);
                    }
                }
                None
            }
            serde_json::Value::Array(items) => items.iter().find_map(walk),
            _ => None,
        }
    }
    walk(&value)
}

fn epoch_to_datetime(epoch: i64) -> chrono::DateTime<chrono::Utc> {
    // codex 端有时给秒，有时给毫秒；> 10^12 视作毫秒。
    let secs = if epoch.abs() > 1_000_000_000_000 {
        epoch / 1000
    } else {
        epoch
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).unwrap_or_else(chrono::Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_access_token_finds_nested() {
        let raw = r#"{
            "email":"a@b",
            "tokens": { "access_token":"tok-1", "refresh_token":"r" }
        }"#;
        assert_eq!(extract_access_token(raw).as_deref(), Some("tok-1"));
    }

    #[test]
    fn extract_access_token_finds_flat() {
        let raw = r#"{ "access_token":"tok-2" }"#;
        assert_eq!(extract_access_token(raw).as_deref(), Some("tok-2"));
    }

    #[test]
    fn extract_access_token_missing_returns_none() {
        assert!(extract_access_token(r#"{"email":"a"}"#).is_none());
    }

    #[test]
    fn epoch_seconds_vs_millis() {
        let s = epoch_to_datetime(1700000000).timestamp();
        let m = epoch_to_datetime(1_700_000_000_000).timestamp();
        assert_eq!(s, m);
        assert_eq!(epoch_to_millis(1_700_000_000), 1_700_000_000_000);
        assert_eq!(epoch_to_millis(1_700_000_000_000), 1_700_000_000_000);
    }

    #[test]
    fn quota_window_minutes_match_codex_windows() {
        assert_eq!(
            quota_window_for_usage_window(Some(300), None),
            QuotaWindow::FiveHour
        );
        assert_eq!(
            quota_window_for_usage_window(Some(10_080), None),
            QuotaWindow::SevenDay
        );
        assert_eq!(
            quota_window_for_usage_window(None, Some(18_000)),
            QuotaWindow::FiveHour
        );
        assert_eq!(
            quota_window_for_usage_window(None, Some(604_800)),
            QuotaWindow::SevenDay
        );
        assert_eq!(
            quota_window_for_usage_window(Some(60), None),
            QuotaWindow::Custom
        );
        assert_eq!(
            quota_window_for_usage_window(None, None),
            QuotaWindow::Custom
        );
    }
}

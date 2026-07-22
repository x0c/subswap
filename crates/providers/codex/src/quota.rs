//! Codex 用量查询：`openai_usage` 请求 + legacy 用量缓存回退。
//!
//! 迁移自旧版 `lib.rs::query_quota` 的方法体（签名从 `&self, id: &AccountId` 改为自由函数
//! `fetch_codex_quota(access_token, account)`，供 [`crate::runtime::CodexRuntime::fetch_quota`]
//! 调用）；`access_token` 现由共享引擎（`FileBlobProvider::query_quota`）统一抽取好再传入，
//! 逻辑本身未变。

use chrono::Utc;

use subswap_core::error::{Error, Result};
use subswap_core::settings;
use subswap_core::time::{epoch_to_datetime, epoch_to_millis};
use subswap_core::{Account, Quota, QuotaStatus, QuotaWindow};

use crate::{app_server, openai_usage};
use crate::{META_AUTH_METADATA, META_CHATGPT_ACCOUNT_ID, PROVIDER_ID};

/// 查询一个 Codex 账号的额度。`access_token` 已由调用方（共享引擎）从 auth blob 中抽好。
pub async fn fetch_codex_quota(access_token: &str, account: &Account) -> Result<Vec<Quota>> {
    // 1. 拿元数据里的 chatgpt_account_id（额度端点必需的 header）。
    let chatgpt_account_id = account
        .extra
        .get(META_CHATGPT_ACCOUNT_ID)
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Error::QuotaFetch(format!(
                "registry entry {PROVIDER_ID}:{} missing {META_CHATGPT_ACCOUNT_ID}; cannot fetch usage",
                account.id
            ))
        })?
        .to_string();

    // 2. 当前账号优先复用官方 app-server 的认证状态。parked 账号没有可安全物化的完整
    // auth blob，继续走旧端点，避免只复制 access token 后遗失 refresh token 轮换结果。
    let raw_resp = if account.active {
        match app_server::fetch_usage().await {
            Ok(usage) => usage,
            Err(error) if app_server::allows_compat_fallback(&error) => {
                tracing::debug!(
                    account = %account.id,
                    error = %error,
                    "Codex 官方额度通道不可用，回退到兼容查询"
                );
                openai_usage::fetch_usage_raw(access_token, &chatgpt_account_id).await?
            }
            Err(error) => {
                return Err(Error::QuotaFetch(format!(
                    "Codex app-server rate-limit query failed: {error}"
                )));
            }
        }
    } else {
        openai_usage::fetch_usage_raw(access_token, &chatgpt_account_id).await?
    };
    let mut normalized = openai_usage::normalize_all(&raw_resp);
    if normalized.iter().all(usage_has_unknown_quota) {
        tracing::debug!(
            account = %account.id,
            shape = %openai_usage::shape_summary(&raw_resp),
            "wham/usage fields unrecognized"
        );
        if let Some(cached_usage) = fresh_cached_legacy_usage(account) {
            tracing::debug!(
                account = %account.id,
                "using fresh legacy usage cache because wham/usage fields were unrecognized"
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
                    (used, 100, QuotaStatus::from_percent(pct))
                }
                (None, Some(u), Some(l)) if l > 0 => {
                    let pct = (u as f64 / l as f64) * 100.0;
                    (u, l, QuotaStatus::from_percent(pct))
                }
                _ => (0, 0, QuotaStatus::Unknown),
            };

            Quota {
                provider: PROVIDER_ID.into(),
                account_id: account.id.clone(),
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

fn usage_has_unknown_quota(usage: &openai_usage::WhamUsage) -> bool {
    usage.used_percent.is_none()
        && usage.percent.is_none()
        && !matches!((usage.used, usage.limit), (Some(_), Some(limit)) if limit > 0)
}

fn fresh_cached_legacy_usage(account: &Account) -> Option<serde_json::Value> {
    let metadata = account.extra.get(META_AUTH_METADATA)?;
    let usage = metadata.get("last_usage")?.clone();
    let cached_at = metadata.get("last_usage_at").and_then(|v| v.as_i64())?;
    let cached_at_ms = epoch_to_millis(cached_at);
    let age_ms = Utc::now().timestamp_millis().saturating_sub(cached_at_ms);
    (age_ms <= settings::current().codex.usage_cache_max_age_ms).then_some(usage)
}

fn quota_window_for_usage_window(minutes: Option<u64>, seconds: Option<u64>) -> QuotaWindow {
    match minutes.or_else(|| seconds.map(|value| value / 60)) {
        Some(300) => QuotaWindow::FiveHour,
        Some(10_080) => QuotaWindow::SevenDay,
        _ => QuotaWindow::Custom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

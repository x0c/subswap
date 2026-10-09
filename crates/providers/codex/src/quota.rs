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

    // 2. 当前账号优先复用官方 app-server 的认证状态。停用号由引擎先经
    // `CodexRuntime::refresh`（OAuth + 完整 auth blob 回写）按需刷新，再走本兼容查询。
    let raw_resp = if account.active {
        match app_server::fetch_usage(&chatgpt_account_id).await {
            Ok(usage) => usage,
            Err(error) if app_server::allows_compat_fallback(&error) => {
                tracing::debug!(
                    account = %account.id,
                    error = %error,
                    "Codex official quota channel unavailable; using compatible query"
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
        .chain(reset_credit_quota(access_token, &chatgpt_account_id, account, &raw_resp).await)
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

/// 重置道具窗口（`QuotaWindow::ResetCredits`）：只读展示，不参与自动切换。
/// `available == 0` → `None`（整列隐藏）；明细失败只降级为「有数量、无过期」，绝不让整份 quota 查询失败。
async fn reset_credit_quota(
    access_token: &str,
    chatgpt_account_id: &str,
    account: &Account,
    raw_resp: &serde_json::Value,
) -> Option<Quota> {
    reset_credit_quota_with_fetch(
        account,
        raw_resp,
        crate::reset_credits::fetch_reset_credits(access_token, chatgpt_account_id),
        std::time::Duration::from_millis(settings::current().codex.reset_details_timeout_ms),
    )
    .await
}

async fn reset_credit_quota_with_fetch(
    account: &Account,
    raw_resp: &serde_json::Value,
    fetch: impl std::future::Future<Output = Result<Vec<crate::reset_credits::ResetCredit>>>,
    budget: std::time::Duration,
) -> Option<Quota> {
    let count = openai_usage::reset_credits_count(raw_resp);
    if count.available == 0 {
        return None;
    }
    let inline = raw_resp
        .get("rate_limit_reset_credits")
        .or_else(|| raw_resp.get("rateLimitResetCredits"));
    let credits = if let Some(node) =
        inline.filter(|node| node.get("credits").is_some_and(serde_json::Value::is_array))
    {
        Some(crate::reset_credits::parse_reset_credits(node))
    } else if account.active || budget.is_zero() {
        // 当前号不得为可选明细挡住主额度；官方响应只有数量时立即展示数量。
        None
    } else {
        match tokio::time::timeout(budget, fetch).await {
            Ok(Ok(credits)) => Some(credits),
            result => {
                tracing::debug!(account = %account.id, timed_out = result.is_err(), "Codex reset credit details unavailable; preserving count");
                None
            }
        }
    };
    let credits = credits.unwrap_or_default();
    // 官方可能裁剪明细，条目数不能覆盖权威计数，也不能声称未知条目不存在更早到期。
    let complete = credits.len() as u64 == count.available;
    let reset_at = complete
        .then(|| credits.iter().filter_map(|c| c.expires_at).min())
        .flatten();
    let mut titles: Vec<&str> = credits.iter().map(|c| c.title.as_str()).collect();
    titles.sort_unstable();
    titles.dedup();
    let note = if titles.is_empty() {
        format!("{} available (details unavailable)", count.available)
    } else {
        format!(
            "{} available: {}{}",
            count.available,
            titles.join("; "),
            if complete {
                ""
            } else {
                " (details incomplete)"
            }
        )
    };
    Some(Quota {
        provider: PROVIDER_ID.into(),
        account_id: account.id.clone(),
        window: QuotaWindow::ResetCredits,
        used: count.available,
        limit: 0,
        reset_at,
        status: QuotaStatus::Ok,
        note: Some(note),
    })
}

fn quota_window_for_usage_window(minutes: Option<u64>, seconds: Option<u64>) -> QuotaWindow {
    match minutes.or_else(|| seconds.map(|value| value / 60)) {
        Some(300) => QuotaWindow::FiveHour,
        Some(10_080) => QuotaWindow::SevenDay,
        // 官方 / 实测月度窗口常见 28–31 天（分钟），例如 43200 / 43800。
        Some(m) if (28 * 1_440..=31 * 1_440).contains(&m) => QuotaWindow::Month,
        _ => QuotaWindow::Custom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(active: bool) -> Account {
        serde_json::from_value(serde_json::json!({
            "provider": "codex", "id": "test", "label": "user@example.com", "active": active,
            "created_at": "2026-10-09T00:00:00Z", "last_used_at": null, "extra": {}
        }))
        .unwrap()
    }

    async fn must_not_fetch() -> Result<Vec<crate::reset_credits::ResetCredit>> {
        panic!("optional HTTP request must not be polled")
    }

    #[tokio::test]
    async fn active_count_only_response_never_waits_for_an_extra_request() {
        let raw = serde_json::json!({"rate_limit_reset_credits": {"available_count": 1}});
        let quota = reset_credit_quota_with_fetch(
            &account(true),
            &raw,
            must_not_fetch(),
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(quota.used, 1);
        assert!(quota.reset_at.is_none());
    }

    #[tokio::test]
    async fn inline_details_are_reused_and_capped_rows_do_not_reduce_the_count() {
        for (count, known_expiry) in [(1, true), (2, false)] {
            let raw = serde_json::json!({"rate_limit_reset_credits": {
                "available_count": count,
                "credits": [{"id": "reset", "status": "available", "expiresAt": 1800000000, "title": "Reset"}]
            }});
            let quota = reset_credit_quota_with_fetch(
                &account(false),
                &raw,
                must_not_fetch(),
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(quota.used, count);
            assert_eq!(quota.reset_at.is_some(), known_expiry);
            if known_expiry {
                assert_eq!(quota.reset_at.unwrap().timestamp(), 1800000000);
            }
        }
    }

    #[tokio::test]
    async fn empty_inline_rows_preserve_nonzero_count_without_fetching_again() {
        let raw =
            serde_json::json!({"rate_limit_reset_credits": {"available_count": 2, "credits": []}});
        let quota = reset_credit_quota_with_fetch(
            &account(false),
            &raw,
            must_not_fetch(),
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(quota.used, 2);
        assert!(quota.reset_at.is_none());
    }

    #[tokio::test]
    async fn zero_count_never_fetches_details() {
        let quota = reset_credit_quota_with_fetch(
            &account(false),
            &serde_json::json!({}),
            must_not_fetch(),
            std::time::Duration::from_secs(1),
        )
        .await;
        assert!(quota.is_none());
    }

    #[tokio::test]
    async fn stalled_parked_details_return_count_within_their_budget() {
        let raw = serde_json::json!({"rate_limit_reset_credits": {"available_count": 3}});
        let quota = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            reset_credit_quota_with_fetch(
                &account(false),
                &raw,
                std::future::pending(),
                std::time::Duration::from_millis(20),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(quota.used, 3);
        assert!(quota.reset_at.is_none());
    }

    #[tokio::test]
    async fn failed_parked_details_do_not_hide_resets() {
        let raw = serde_json::json!({"rate_limit_reset_credits": {"available_count": 2}});
        let fetch = async { Err(Error::QuotaFetch("HTTP 429 rate limited".into())) };
        let quota = reset_credit_quota_with_fetch(
            &account(false),
            &raw,
            fetch,
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(quota.used, 2);
        assert!(quota.reset_at.is_none());
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
        assert_eq!(
            quota_window_for_usage_window(Some(43_200), None),
            QuotaWindow::Month
        );
        assert_eq!(
            quota_window_for_usage_window(Some(43_800), None),
            QuotaWindow::Month
        );
        assert_eq!(
            quota_window_for_usage_window(None, Some(43_200 * 60)),
            QuotaWindow::Month
        );
    }
}

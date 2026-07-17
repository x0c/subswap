//! Kimi usage 查询：GET {base}/usages。数值为字符串、reset 为 ISO8601。
//! usage → 7d 窗口；limits[].window(duration+timeUnit) 换算分钟：300→5h、10080→7d、其余 Custom。

use chrono::{DateTime, Utc};
use subswap_core::error::{Error, Result};
use subswap_core::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};

fn base_url() -> String {
    std::env::var("KIMI_CODE_BASE_URL")
        .unwrap_or_else(|_| "https://api.kimi.com/coding/v1".into())
        .trim_end_matches('/')
        .to_string()
}

fn to_u64(v: Option<&serde_json::Value>) -> Option<u64> {
    let v = v?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.parse().ok())
}

fn reset_at(detail: &serde_json::Value) -> Option<DateTime<Utc>> {
    let s = detail.get("resetTime")?.as_str()?;
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

fn window_from_minutes(minutes: u64) -> QuotaWindow {
    match minutes {
        300 => QuotaWindow::FiveHour,
        10_080 => QuotaWindow::SevenDay,
        _ => QuotaWindow::Custom,
    }
}

fn minutes_of(window: &serde_json::Value) -> Option<u64> {
    let duration = to_u64(window.get("duration"))?;
    let unit = window.get("timeUnit").and_then(|v| v.as_str()).unwrap_or("");
    let mins = match unit {
        u if u.contains("MINUTE") => duration,
        u if u.contains("HOUR") => duration * 60,
        u if u.contains("DAY") => duration * 60 * 24,
        _ => duration,
    };
    Some(mins)
}

fn quota_from(detail: &serde_json::Value, window: QuotaWindow, provider: &str, id: &AccountId) -> Option<Quota> {
    let limit = to_u64(detail.get("limit"))?;
    let used = to_u64(detail.get("used")).or_else(|| {
        let rem = to_u64(detail.get("remaining"))?;
        Some(limit.saturating_sub(rem))
    })?;
    let pct = if limit > 0 { used as f64 / limit as f64 * 100.0 } else { 0.0 };
    Some(Quota {
        provider: provider.into(),
        account_id: id.clone(),
        window,
        used,
        limit,
        reset_at: reset_at(detail),
        status: QuotaStatus::from_percent(pct),
        note: None,
    })
}

/// 解析 /usages 响应为多窗口 Quota。
pub fn parse_usages(body: &str, provider: &str, id: &AccountId) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Some(usage) = v.get("usage") {
        if let Some(q) = quota_from(usage, QuotaWindow::SevenDay, provider, id) {
            out.push(q);
        }
    }
    if let Some(limits) = v.get("limits").and_then(|x| x.as_array()) {
        for item in limits {
            let window = item
                .get("window")
                .and_then(minutes_of)
                .map(window_from_minutes)
                .unwrap_or(QuotaWindow::Custom);
            let detail = item.get("detail").unwrap_or(item);
            if let Some(q) = quota_from(detail, window, provider, id) {
                out.push(q);
            }
        }
    }
    out
}

/// 调 /usages 端点。
pub async fn fetch_quota(access_token: &str, account: &Account) -> Result<Vec<Quota>> {
    let url = format!("{}/usages", base_url());
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", "subswap")
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("kimi usages request failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(Error::QuotaFetch(format!("kimi usages HTTP {status}")));
    }
    Ok(parse_usages(&body, "kimi", &account.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = r#"{
      "usage": {"limit":"100","used":"4","remaining":"96","resetTime":"2026-07-24T07:52:15Z"},
      "limits": [{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
                  "detail":{"limit":"100","used":"18","remaining":"82","resetTime":"2026-07-17T12:52:15Z"}}]
    }"#;

    #[test]
    fn parses_weekly_and_5h() {
        let q = parse_usages(REAL, "kimi", &AccountId("u1".into()));
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].window, QuotaWindow::SevenDay);
        assert_eq!((q[0].used, q[0].limit), (4, 100));
        assert_eq!(q[1].window, QuotaWindow::FiveHour);
        assert_eq!((q[1].used, q[1].limit), (18, 100));
        assert!(q[1].reset_at.is_some());
    }

    #[test]
    fn used_derived_from_remaining_when_absent() {
        let body = r#"{"usage":{"limit":"100","remaining":"70","resetTime":"2026-07-24T07:52:15Z"}}"#;
        let q = parse_usages(body, "kimi", &AccountId("u1".into()));
        assert_eq!((q[0].used, q[0].limit), (30, 100));
    }
}

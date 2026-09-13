//! Command Code 额度：GET `{base}/alpha/billing/credits`，Bearer API key。
//!
//! 该路径是官方 CLI `/usage` 实际在用的 undocumented `/alpha/*` 接口，
//! 不是公开 Provider API 契约；解析必须宽容。字段语义：
//! - `windowLimits.fiveHour|weekly`：`used`/`cap` 为美元用量，转成已用百分比
//! - `credits.monthlyCredits` + `purchasedCredits` + `freeCredits`：余量美元 → Credits（分）

use chrono::{DateTime, TimeZone, Utc};
use subswap_core::error::{Error, Result};
use subswap_core::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};

const DEFAULT_BASE: &str = "https://api.commandcode.ai";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn base_url() -> String {
    std::env::var("SUBSWAP_COMMANDCODE_BASE")
        .unwrap_or_else(|_| DEFAULT_BASE.into())
        .trim_end_matches('/')
        .to_string()
}

fn to_f64(v: Option<&serde_json::Value>) -> Option<f64> {
    let v = v?;
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    if let Some(n) = v.as_i64() {
        return Some(n as f64);
    }
    v.as_str()?.parse().ok()
}

fn reset_at_millis(v: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    let ms = to_f64(v)? as i64;
    if ms <= 0 {
        return None;
    }
    Utc.timestamp_millis_opt(ms).single()
}

fn window_percent(window: &serde_json::Value) -> Option<f64> {
    if window.get("exceeded").and_then(|v| v.as_bool()) == Some(true) {
        return Some(100.0);
    }
    let used = to_f64(window.get("used"))?;
    let cap = to_f64(window.get("cap"))?;
    if !used.is_finite() || !cap.is_finite() {
        return None;
    }
    if cap <= 0.0 {
        // Provider 套餐等无窗口上限：不当成耗尽。
        return Some(0.0);
    }
    Some(((used / cap) * 100.0).clamp(0.0, 100.0))
}

fn quota_from_window(
    window: &serde_json::Value,
    kind: QuotaWindow,
    provider: &str,
    id: &AccountId,
) -> Option<Quota> {
    let pct = window_percent(window)?;
    let used = pct.round().clamp(0.0, 100.0) as u64;
    Some(Quota {
        provider: provider.into(),
        account_id: id.clone(),
        window: kind,
        used,
        limit: 100,
        reset_at: reset_at_millis(window.get("resetAt")),
        status: QuotaStatus::from_percent(pct),
        note: None,
    })
}

fn credits_remaining_cents(credits: &serde_json::Value) -> Option<u64> {
    let monthly = to_f64(credits.get("monthlyCredits")).unwrap_or(0.0);
    let purchased = to_f64(credits.get("purchasedCredits")).unwrap_or(0.0);
    let free = to_f64(credits.get("freeCredits")).unwrap_or(0.0);
    let total = monthly + purchased + free;
    if !total.is_finite() {
        return None;
    }
    Some((total * 100.0).round().clamp(0.0, u64::MAX as f64) as u64)
}

fn credits_quota(provider: &str, id: &AccountId, remaining_cents: u64) -> Quota {
    // 未知套餐总额时：有余量 → used=0/limit=remaining；耗尽 → 1/1（Exhausted + $0.00）。
    let (used, limit) = if remaining_cents == 0 {
        (1, 1)
    } else {
        (0, remaining_cents)
    };
    let pct = if limit == 0 {
        0.0
    } else {
        (used as f64 / limit as f64) * 100.0
    };
    Quota {
        provider: provider.into(),
        account_id: id.clone(),
        window: QuotaWindow::Credits,
        used,
        limit,
        reset_at: None,
        status: QuotaStatus::from_percent(pct),
        note: None,
    }
}

/// 解析 `/alpha/billing/credits` 响应。
pub fn parse_credits(body: &str, provider: &str, id: &AccountId) -> Vec<Quota> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Some(limits) = v.get("windowLimits") {
        if let Some(w) = limits.get("fiveHour") {
            if let Some(q) = quota_from_window(w, QuotaWindow::FiveHour, provider, id) {
                out.push(q);
            }
        }
        if let Some(w) = limits.get("weekly") {
            if let Some(q) = quota_from_window(w, QuotaWindow::SevenDay, provider, id) {
                out.push(q);
            }
        }
    }
    if let Some(credits) = v.get("credits") {
        if let Some(cents) = credits_remaining_cents(credits) {
            // 与 Cursor 一致：完全没有额度账本时仍画 `$0.00`（本接口显式返回了 credits 块）。
            out.push(credits_quota(provider, id, cents));
        }
    }
    out
}

/// 用 Command Code API key 查额度。
pub async fn fetch_quota(access_token: &str, account: &Account) -> Result<Vec<Quota>> {
    fetch_quota_at(access_token, account, &base_url()).await
}

async fn fetch_quota_at(
    access_token: &str,
    account: &Account,
    api_base: &str,
) -> Result<Vec<Quota>> {
    let url = format!("{}/alpha/billing/credits", api_base.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| Error::QuotaFetch(format!("commandcode credits client failed: {e}")))?;
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header(
            "User-Agent",
            format!("subswap/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|e| Error::QuotaFetch(format!("commandcode credits request failed: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(Error::QuotaFetch(format!(
            "commandcode credits HTTP {status}: needs re-login: {body}"
        )));
    }
    if !status.is_success() {
        return Err(Error::QuotaFetch(format!(
            "commandcode credits HTTP {status}: {body}"
        )));
    }
    Ok(parse_credits(&body, crate::PROVIDER_ID, &account.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "credits": { "monthlyCredits": 8.68, "purchasedCredits": 1.32, "freeCredits": 0 },
      "windowLimits": {
        "fiveHour": { "used": 0.96, "cap": 3, "exceeded": false, "resetAt": 1786775976124 },
        "weekly": { "used": 2.46, "cap": 6, "exceeded": false, "resetAt": 1787310657649 }
      }
    }"#;

    #[test]
    fn parses_windows_and_credits() {
        let q = parse_credits(SAMPLE, "commandcode", &AccountId("cc-abc".into()));
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].window, QuotaWindow::FiveHour);
        assert_eq!(q[0].used, 32); // 0.96/3 ≈ 32%
        assert!(q[0].reset_at.is_some());
        assert_eq!(q[1].window, QuotaWindow::SevenDay);
        assert_eq!(q[1].used, 41); // 2.46/6 = 41%
        assert_eq!(q[2].window, QuotaWindow::Credits);
        assert_eq!(q[2].used, 0);
        assert_eq!(q[2].limit, 1000); // $10.00
    }

    #[test]
    fn exceeded_is_full() {
        let body = r#"{"windowLimits":{"fiveHour":{"used":1,"cap":3,"exceeded":true,"resetAt":1}}}"#;
        let q = parse_credits(body, "commandcode", &AccountId("x".into()));
        assert_eq!(q[0].used, 100);
        assert_eq!(q[0].status, QuotaStatus::Exhausted);
    }

    #[test]
    fn zero_credits_is_exhausted() {
        let body = r#"{"credits":{"monthlyCredits":0,"purchasedCredits":0,"freeCredits":0}}"#;
        let q = parse_credits(body, "commandcode", &AccountId("x".into()));
        assert_eq!(q[0].window, QuotaWindow::Credits);
        assert_eq!(q[0].status, QuotaStatus::Exhausted);
        assert_eq!((q[0].used, q[0].limit), (1, 1));
    }

    #[test]
    fn uncapped_window_is_not_exhausted() {
        let body = r#"{"windowLimits":{"fiveHour":{"used":1.5,"cap":0,"exceeded":false}}}"#;
        let q = parse_credits(body, "commandcode", &AccountId("x".into()));
        assert_eq!(q[0].used, 0);
        assert_eq!(q[0].status, QuotaStatus::Ok);
    }

    fn sample_account() -> Account {
        Account {
            provider: "commandcode".into(),
            id: AccountId("cc-test".into()),
            label: "cc-…test".into(),
            active: true,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    struct MockServer {
        base_url: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockServer {
        fn start(responses: Vec<(&'static str, &'static str)>) -> Self {
            use std::io::{Read, Write};
            use std::net::TcpListener;

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = std::sync::Arc::clone(&requests);
            let handle = std::thread::spawn(move || {
                for (status, body) in responses {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut buffer = [0_u8; 8192];
                    let count = stream.read(&mut buffer).unwrap();
                    let request = String::from_utf8_lossy(&buffer[..count]);
                    captured
                        .lock()
                        .unwrap()
                        .push(request.lines().next().unwrap_or_default().to_string());
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            Self {
                base_url,
                requests,
                handle: Some(handle),
            }
        }

        fn base_url(&self) -> &str {
            &self.base_url
        }

        fn finish(mut self) -> Vec<String> {
            self.handle.take().unwrap().join().unwrap();
            std::sync::Arc::try_unwrap(self.requests)
                .unwrap()
                .into_inner()
                .unwrap()
        }
    }

    #[tokio::test]
    async fn fetch_maps_windows_from_http() {
        let server = MockServer::start(vec![("200 OK", SAMPLE)]);
        let quotas = fetch_quota_at("cc-live", &sample_account(), server.base_url())
            .await
            .unwrap();
        assert_eq!(quotas.len(), 3);
        assert_eq!(quotas[0].window, QuotaWindow::FiveHour);
        assert_eq!(
            server.finish(),
            vec!["GET /alpha/billing/credits HTTP/1.1".to_string()]
        );
    }

    #[tokio::test]
    async fn fetch_401_is_authentication_failure() {
        let server =
            MockServer::start(vec![("401 Unauthorized", r#"{"error":"invalid_api_key"}"#)]);
        let err = fetch_quota_at("cc-dead", &sample_account(), server.base_url())
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            subswap_core::is_authentication_failure(&text),
            "401 must block auto-swap candidates: {text}"
        );
        assert!(text.contains("needs re-login"), "{text}");
        let _ = server.finish();
    }

    #[tokio::test]
    async fn fetch_429_is_not_authentication_failure() {
        let server = MockServer::start(vec![("429 Too Many Requests", r#"{"error":"rate_limited"}"#)]);
        let err = fetch_quota_at("cc-live", &sample_account(), server.base_url())
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            !subswap_core::is_authentication_failure(&text),
            "429 must stay a transient fetch failure: {text}"
        );
        let _ = server.finish();
    }
}
